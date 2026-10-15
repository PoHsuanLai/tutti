//! IParameterChanges COM implementation.
//!
//! # Real-time safety
//!
//! The inner `Vec<ComWrapper<ParamValueQueueImpl>>` lives in an
//! [`AudioThreadCell`], mirroring
//! [`super::event_list::EventList`] and
//! [`super::param_queue::ParamValueQueueImpl`].
//!
//! [`refill_from_changes`](ParameterChangesImpl::refill_from_changes)
//! lets a host reuse a single `ComWrapper<ParameterChangesImpl>` across
//! buffers. The queues form a pool that only grows: a block uses the first
//! `live` of them and the rest stay allocated, so a block with fewer queues
//! than the last frees nothing and the next block with more allocates
//! nothing. (The host sends changes only, so the queue count varies from
//! block to block; trimming the pool to each block's count would free and
//! reallocate queues every time it did.) The pool is sized for every
//! parameter at construction ([`with_queues`](ParameterChangesImpl::with_queues)),
//! the way the SDK's own host reserves one queue per parameter
//! (`ParameterChanges::setMaxParameters`).
//!
//! **The host's input list pools its points**
//! ([`with_pool`](ParameterChangesImpl::with_pool)) in the carried layout
//! the host's chunk has (`ParameterChanges::reserved_carried`): each queue
//! owns `ParameterChanges::FLOOR_POINTS` points and one more for the value
//! a skipped block carries, and a queue refilled with more borrows a block
//! of the full room (and that one more) from the list's shared
//! [`PointPool`], the same pool type the host's sets use, so the list's
//! memory follows the points a chunk carries rather than the parameter
//! count. Every queue's points are a fixed-capacity [`PointBuf`]
//! (`tutti_plugin_types::PointBuf`): past a block, or with the pool spent,
//! the queue keeps the parameter's latest value, counted
//! ([`points_dropped`](ParameterChangesImpl::points_dropped)); nothing grows.

use vst3::Steinberg::Vst::{IParamValueQueue, IParameterChanges, IParameterChangesTrait};
use vst3::{Class, ComWrapper};

use super::param_queue::ParamValueQueueImpl;
use crate::types::ParameterChanges;
use tutti_plugin_types::{ParamAddress, PointPool};
use tutti_types::AudioThreadCell;

pub struct ParameterChangesImpl {
    /// The pool of queues; the first `live` are this block's.
    queues: AudioThreadCell<Vec<ComWrapper<ParamValueQueueImpl>>>,
    /// How many of `queues` hold this block's changes.
    live: AudioThreadCell<usize>,
    /// The shared point pool, lent to `queues` by index (a
    /// [`with_pool`](Self::with_pool) list; no blocks otherwise).
    pool: AudioThreadCell<PointPool>,
    /// Points a refill could not hand on whole.
    dropped: AudioThreadCell<u64>,
}

impl Class for ParameterChangesImpl {
    type Interfaces = (IParameterChanges,);
}

impl ParameterChangesImpl {
    /// An empty list with no pooled queues (tests; the host presizes).
    #[cfg(test)]
    pub fn new_empty() -> ComWrapper<Self> {
        Self::with_queues(0, 0)
    }

    /// An empty changes list whose pool already holds `queues` queues, each
    /// with room for `points` points, so no block with up to that many
    /// queues of up to that many points allocates. Allocates; call it off
    /// the audio thread with the plugin's parameter count. Every queue holds
    /// its full room, so the host's input list is built with
    /// [`with_pool`](Self::with_pool) instead, which falls back to this when
    /// the pool would not save storage. The output list, which the plugin
    /// fills, has no per-chunk figure and passes
    /// [`MIN_POINT_ROOM`](super::MIN_POINT_ROOM), the least room any queue
    /// is built with.
    pub fn with_queues(queues: usize, points: usize) -> ComWrapper<Self> {
        ComWrapper::new(Self {
            // Placeholder param ids: each queue is refilled before it is live.
            queues: AudioThreadCell::new(
                (0..queues)
                    .map(|_| ParamValueQueueImpl::with_room(0, points))
                    .collect(),
            ),
            live: AudioThreadCell::new(0),
            pool: AudioThreadCell::new(PointPool::default()),
            dropped: AudioThreadCell::new(0),
        })
    }

    /// The host's input list: `queues` queues for a chunk whose queues carry
    /// up to `room` points each (`ParameterQueue::chunk_room` at the
    /// activated block size), stored as the host's chunk stores them, each
    /// with one more point for the value a skipped block carries
    /// ([`ParameterChanges::carried_budget`], the layout of
    /// [`ParameterChanges::reserved_carried`]): each queue owns
    /// [`ParameterChanges::FLOOR_POINTS`] + 1, and any
    /// [`ParameterChanges::POOLED_QUEUES`] of them at once borrow a block of
    /// `room + 1` from the list's shared [`PointPool`]. The extra floor point is what
    /// keeps a floor queue the bridge prepended a carried value to from
    /// borrowing a block a full queue needs. With few enough queues every
    /// queue has `room + 1` instead, as [`with_queues`](Self::with_queues)
    /// gives it. A refill never allocates within that room. Allocates; call
    /// it off the audio thread.
    pub fn with_pool(queues: usize, room: usize) -> ComWrapper<Self> {
        let (floor, blocks) = ParameterChanges::point_layout(queues, room);
        let (floor, points) = (floor + 1, room + 1);
        if blocks == 0 {
            return Self::with_queues(queues, points);
        }
        ComWrapper::new(Self {
            queues: AudioThreadCell::new(
                (0..queues)
                    .map(|_| ParamValueQueueImpl::with_exact_room(0, floor))
                    .collect(),
            ),
            live: AudioThreadCell::new(0),
            pool: AudioThreadCell::new(PointPool::new(blocks, points)),
            dropped: AudioThreadCell::new(0),
        })
    }

    /// Points the list could not hand on whole since it was built: a
    /// refill's points past a queue's room, or past its own floor with the
    /// pool spent, and the plugin's `addPoint` past a queue's room (the
    /// output list), each point that gave way to its parameter's latest
    /// value. Zero while the host (or the plugin) stays within the room the
    /// list was built for.
    pub fn points_dropped(&self) -> u64 {
        let added: u64 = self
            .queues
            .borrow()
            .iter()
            .map(|q| q.points_dropped())
            .sum();
        *self.dropped.borrow() + added
    }

    /// Every point the list has storage for, in its queues and its pool.
    #[cfg(test)]
    pub fn point_storage(&self) -> usize {
        let queues: usize = self.queues.borrow().iter().map(|q| q.capacity()).sum();
        queues + self.pool.borrow().storage()
    }

    /// Takes back every block lent for the last block, giving each queue its
    /// floor storage back ([`PointPool::give_back`]). Allocation-free.
    fn return_blocks(&self) {
        let queues = self.queues.borrow();
        self.pool
            .borrow_mut()
            .give_back(|i, own| match queues.get(i) {
                Some(q) => {
                    q.swap_points(own);
                    true
                }
                None => false,
            });
    }

    /// Refill this changes list from `changes` in place, reusing the pool's
    /// queues.
    ///
    /// # Real-time
    ///
    /// Allocation-free when the pool holds at least `changes.queues.len()`
    /// queues. Past the pool it grows once (a queue, not its points) and
    /// keeps the larger size; it never shrinks.
    ///
    /// No queue's points ever grow: a queue that needs more than its own
    /// room borrows a block from a [`with_pool`](Self::with_pool) list's
    /// shared pool until the next refill, and past the block (or with the
    /// pool spent, or with no pool) the queue keeps the parameter's latest
    /// value, counted ([`points_dropped`](Self::points_dropped)).
    pub fn refill_from_changes(&self, changes: &ParameterChanges) {
        self.return_blocks();
        {
            let mut queues = self.queues.borrow_mut();
            while queues.len() < changes.queues.len() {
                // Placeholder param_id — refilled immediately below.
                queues.push(ParamValueQueueImpl::new_empty(0));
            }
        }
        let queues = self.queues.borrow();
        let mut pool = self.pool.borrow_mut();
        let mut dropped = 0;
        for (i, (slot, q)) in queues.iter().zip(changes.queues.iter()).enumerate() {
            if q.points.len() > slot.capacity() {
                // A block for the block, when the list has one left; the
                // queue's own storage (refilled below anyway) is kept to
                // swap back.
                let _ = slot.lend_from(&mut pool, i);
            }
            dropped += slot.refill_from_queue(q);
        }
        *self.dropped.borrow_mut() += dropped;
        *self.live.borrow_mut() = changes.queues.len();
    }

    /// Empties the list for the next block. The queues stay in the pool, so
    /// this frees nothing and never allocates.
    pub fn clear_in_place(&self) {
        self.return_blocks();
        *self.live.borrow_mut() = 0;
    }

    /// Drains the plugin's emitted parameter changes into a caller-supplied
    /// pooled [`ParameterChanges`]. Clears `out` first with
    /// [`ParameterChanges::clear`], which hands its queues back to its spares
    /// with their point storage kept, so for an `out` built
    /// [`ParameterChanges::reserved`] this never allocates: past a queue's
    /// room with `out`'s pool spent, the queue keeps the parameter's latest
    /// value, counted in [`ParameterChanges::points_overflowed`].
    pub fn fill_changes(&self, out: &mut ParameterChanges) {
        out.clear();

        let live = *self.live.borrow();
        for queue in self.queues.borrow().iter().take(live) {
            // Outbound across the COM edge: the plugin answered with a bare
            // `ParamID`, and VST3 ids are opaque — tag it rather than letting
            // the number travel with its model unstated.
            let param_id = ParamAddress::Opaque(queue.param_id().into());
            queue.for_each_point(|point| {
                out.add_change(param_id, point.sample_offset, point.value.get());
            });
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        *self.live.borrow()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many queues the pool holds, live or not.
    #[cfg(test)]
    pub fn pooled(&self) -> usize {
        self.queues.borrow().len()
    }
}

impl IParameterChangesTrait for ParameterChangesImpl {
    unsafe fn getParameterCount(&self) -> i32 {
        i32::try_from(*self.live.borrow()).unwrap_or(i32::MAX)
    }

    unsafe fn getParameterData(&self, index: i32) -> *mut IParamValueQueue {
        let queues = self.queues.borrow();
        let live = *self.live.borrow();
        if index < 0 || index as usize >= live {
            return std::ptr::null_mut();
        }
        // Borrowed pointer: VST3 plugins do not release queues returned here.
        match queues[index as usize].as_com_ref::<IParamValueQueue>() {
            Some(r) => r.as_ptr(),
            None => std::ptr::null_mut(),
        }
    }

    unsafe fn addParameterData(&self, id: *const u32, index: *mut i32) -> *mut IParamValueQueue {
        if id.is_null() {
            return std::ptr::null_mut();
        }
        let param_id = *id;
        let mut queues = self.queues.borrow_mut();
        let mut live = self.live.borrow_mut();
        // The next pooled queue, emptied for `param_id`; a new one only past
        // the pool.
        if *live < queues.len() {
            queues[*live].reset(param_id);
        } else {
            queues.push(ParamValueQueueImpl::new_empty(param_id));
        }
        let queue_ptr = queues[*live]
            .as_com_ref::<IParamValueQueue>()
            .map(|r| r.as_ptr())
            .unwrap_or(std::ptr::null_mut());
        if !index.is_null() {
            *index = i32::try_from(*live).unwrap_or(i32::MAX);
        }
        *live += 1;
        queue_ptr
    }
}

/// Raw `IParameterChanges*` for `ProcessData`, or null if the wrapper can't
/// expose the interface. Used by the realtime `process` path for both the
/// input and output parameter-change slots.
pub fn param_changes_ptr(changes: &ComWrapper<ParameterChangesImpl>) -> *mut IParameterChanges {
    changes
        .as_com_ref::<IParameterChanges>()
        .map(|r| r.as_ptr())
        .unwrap_or(std::ptr::null_mut())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changes_with(num_queues: usize, points_per: usize) -> ParameterChanges {
        let mut changes = ParameterChanges::reserved(num_queues, points_per);
        for q in 0..num_queues {
            for p in 0..points_per {
                changes.add_change(
                    ParamAddress::Opaque((q as u32).into()),
                    p as i32,
                    p as f64 * 0.01,
                );
            }
        }
        changes
    }

    /// RT regression: once `refill_from_changes` has seen N queues,
    /// subsequent refills at ≤ N queues must not allocate. This is the
    /// steady-state automation path.
    #[test]
    fn refill_from_changes_steady_state_is_allocation_free() {
        let wrapper = ParameterChangesImpl::new_empty();
        let changes = changes_with(8, 4);

        // Grow the pool once (off the hot path).
        wrapper.refill_from_changes(&changes);

        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..1_000 {
                wrapper.refill_from_changes(&changes);
            }
        });
        assert_eq!(wrapper.len(), 8);
    }

    /// **A block with fewer queues than the last frees nothing, and the next
    /// with more allocates nothing**: the host sends changes only, so the
    /// queue count varies block to block, and a list sized for every
    /// parameter must ride that out without touching the allocator. The
    /// plugin sees exactly this block's queues.
    ///
    /// Mutation (run): trimming the pool to each block's count
    /// (`queues.truncate(changes.queues.len())` after the refill, as before
    /// this change) → the gate trips on the block that grows again.
    #[test]
    fn a_varying_queue_count_never_touches_the_allocator() {
        use vst3::Steinberg::Vst::{IParameterChanges, IParameterChangesTrait};

        let wrapper = ParameterChangesImpl::with_queues(200, 0);
        let ptr = wrapper.to_com_ptr::<IParameterChanges>().unwrap();
        let sizes = [200, 3, 0, 150, 1, 200, 17];
        let blocks: Vec<ParameterChanges> = sizes.iter().map(|&n| changes_with(n, 1)).collect();

        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..100 {
                for (changes, &n) in blocks.iter().zip(&sizes) {
                    if changes.is_empty() {
                        wrapper.clear_in_place();
                    } else {
                        wrapper.refill_from_changes(changes);
                    }
                    assert_eq!(unsafe { ptr.getParameterCount() }, n as i32);
                }
            }
        });
        assert_eq!(wrapper.pooled(), 200);
        // A queue past the live count is not reachable by the plugin.
        assert!(unsafe { ptr.getParameterData(17) }.is_null());
    }

    /// **A list presized for the chunk's points refills without allocating
    /// on first use**: every queue already has the room, so the first block
    /// that carries 140 points on each of 50 parameters reserves nothing
    /// (it used to grow each queue past its 16 points on the audio thread).
    ///
    /// Mutation (run): `with_room` ignoring `points` (a room of
    /// `MIN_POINT_ROOM`) → each queue keeps its latest value past that
    /// room, not its 140 points → fails. (Reworded with `PointBuf`: the
    /// first refill grew each queue inside the gate before.)
    #[test]
    fn a_presized_list_takes_its_first_dense_block_without_allocating() {
        let wrapper = ParameterChangesImpl::with_queues(50, 140);
        let block = changes_with(50, 140);
        assert_no_alloc::assert_no_alloc(|| {
            wrapper.refill_from_changes(&block);
        });
        assert_eq!(wrapper.len(), 50);
        assert!(wrapper.queues.borrow().iter().all(|q| q.len() == 140));
    }

    /// The plugin's own `addParameterData` on the output list reuses the
    /// pooled queues block after block: each block's queues start empty and
    /// under the id the plugin asked for, and none is allocated.
    ///
    /// Mutation (run): `addParameterData` without `reset` (a reused queue
    /// keeps last block's points and id) → the point count and id fail.
    #[test]
    fn add_parameter_data_reuses_pooled_queues() {
        use vst3::Steinberg::Vst::{
            IParamValueQueueTrait, IParameterChanges, IParameterChangesTrait,
        };

        let changes = ParameterChangesImpl::with_queues(4, 0);
        let ptr = changes.to_com_ptr::<IParameterChanges>().unwrap();
        let block = |id: u32| unsafe {
            changes.clear_in_place();
            let mut index = -1;
            let q = ptr.addParameterData(&id, &mut index);
            let q = vst3::ComRef::from_raw(q).unwrap();
            let mut at = -1;
            assert_eq!(q.getPointCount(), 0);
            q.addPoint(0, 0.5, &mut at);
            assert_eq!(index, 0);
            assert_eq!(q.getParameterId(), id);
        };
        block(1);
        assert_no_alloc::assert_no_alloc(|| {
            for id in 2..100 {
                block(id);
            }
        });
        // Reserved: `fill_changes` fills within the set's room.
        let mut out = ParameterChanges::reserved(4, 1);
        changes.fill_changes(&mut out);
        assert_eq!(out.queues.len(), 1);
        assert_eq!(out.queues[0].points.len(), 1);
        assert_eq!(changes.pooled(), 4);
    }

    /// `clear_in_place` on an already-empty wrapper is allocation-free
    /// (the hot-path case once the plugin isn't emitting output param
    /// events). Exercised rather than the "had items, now empty" case
    /// because the latter drops `ComWrapper` refcounts which allocate.
    #[test]
    fn clear_in_place_on_empty_is_allocation_free() {
        let wrapper = ParameterChangesImpl::new_empty();

        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..10_000 {
                wrapper.clear_in_place();
            }
        });
        assert!(wrapper.is_empty());
    }

    /// A plugin that wants to write automation back calls `addParameterData`
    /// on the host's list and then writes into the queue it is handed. The
    /// vtable must therefore hand back a live queue *and* the index it was
    /// filed under — a null pointer or a stale index is a plugin crash, not a
    /// missed value.
    #[test]
    fn add_parameter_data_hands_the_plugin_a_live_queue_and_its_index() {
        use vst3::Steinberg::Vst::{IParameterChanges, IParameterChangesTrait};

        let changes = ParameterChangesImpl::new_empty();
        let ptr = changes.to_com_ptr::<IParameterChanges>().unwrap();

        unsafe {
            assert_eq!(ptr.getParameterCount(), 0);

            let param_id: u32 = 42;
            let mut index: i32 = -1;
            let queue_ptr = ptr.addParameterData(&param_id, &mut index);
            assert!(!queue_ptr.is_null(), "plugin must get a writable queue");
            assert_eq!(index, 0);
            assert_eq!(ptr.getParameterCount(), 1);

            assert!(
                !ptr.getParameterData(0).is_null(),
                "the queue must be retrievable at the index we reported"
            );
        }
    }

    /// **The plugin's emitted changes drain into the reserved output set
    /// without allocating, past its pool too.** 1 000 parameters, the output
    /// set reserved as activation reserves it (`MIN_POINT_ROOM` points a
    /// queue, so pooled). Warmed with one pattern (parameters `0..256`
    /// emitting a full room, every other one two points), then measured
    /// with another inside the no-alloc gate (parameters `256..512` at a
    /// full room, so no warm-up left them storage, and one more parameter
    /// emitting a third point with the pool spent): nothing grows, that
    /// parameter's third point takes its last point's place (it ends on its
    /// latest value), and one point is counted.
    ///
    /// Mutation (run): `ParameterChanges::push_point` dropping the point
    /// past the room (`try_push` only, no `replace_latest`) → the third
    /// point is lost and the parameter ends on its second → fails.
    /// (Reworded with `PointBuf`: the unbounded path, which grew a floor
    /// inside the gate, is gone.)
    #[test]
    fn emitted_changes_drain_into_the_reserved_set_without_allocating() {
        use vst3::Steinberg::Vst::{IParamValueQueueTrait, IParameterChanges};

        const PARAMS: u32 = 1_000;
        let room = super::super::MIN_POINT_ROOM;
        let pooled = ParameterChanges::POOLED_QUEUES as u32;
        let list = ParameterChangesImpl::with_queues(PARAMS as usize, room);
        let ptr = list.to_com_ptr::<IParameterChanges>().unwrap();
        let mut out = ParameterChanges::reserved(PARAMS as usize, room);
        assert!(
            ParameterChanges::point_layout(PARAMS as usize, room).1 > 0,
            "pooled"
        );
        let emit = |moving: core::ops::Range<u32>, third: Option<u32>| unsafe {
            list.clear_in_place();
            for id in 0..PARAMS {
                let n = if moving.contains(&id) {
                    room
                } else if third == Some(id) {
                    ParameterChanges::FLOOR_POINTS + 1
                } else {
                    ParameterChanges::FLOOR_POINTS
                };
                let mut index = -1;
                let q = vst3::ComRef::from_raw(ptr.addParameterData(&id, &mut index)).unwrap();
                for k in 0..n {
                    let mut at = -1;
                    q.addPoint(k as i32, (k as f64) / 32.0, &mut at);
                }
            }
        };
        emit(0..pooled, None);
        list.fill_changes(&mut out);
        let storage = out.point_storage();
        assert_no_alloc::assert_no_alloc(|| {
            emit(pooled..2 * pooled, Some(2 * pooled));
            list.fill_changes(&mut out);
        });
        assert_eq!(out.point_storage(), storage);
        assert_eq!(out.points_overflowed(), 1);
        for q in &out.queues {
            let id = q.param_id.opaque().expect("opaque").get();
            let want = if (pooled..2 * pooled).contains(&id) {
                room
            } else {
                ParameterChanges::FLOOR_POINTS
            };
            assert_eq!(q.points.len(), want, "parameter {id}");
        }
        let third = out
            .get_queue(ParamAddress::Opaque((2 * pooled).into()))
            .expect("queued");
        let last = ParameterChanges::FLOOR_POINTS as i32;
        assert_eq!(third.points.last().map(|p| p.sample_offset), Some(last));
    }

    /// The host's chunk at the pool budget: `params` parameters with a room
    /// of `room`, the first `POOLED_QUEUES` at full room and the rest at
    /// their floor, the points arriving interleaved (frame by frame, every
    /// parameter) into a pooled set built as the host's node builds it
    /// (`reserved_bridged`), as the host's node and the wire's deserialize
    /// hand it on. The parameters moving at full room are
    /// `first..first + POOLED_QUEUES`. Point `k` of parameter `id` is on
    /// frame `2k` with value `(id + k) % 5 / 8`.
    fn pooled_chunk(params: u32, room: usize, first: u32) -> ParameterChanges {
        let mut changes = ParameterChanges::reserved_bridged(params as usize, room);
        let moving = first..first + ParameterChanges::POOLED_QUEUES as u32;
        for k in 0..room {
            for id in 0..params {
                if k >= ParameterChanges::FLOOR_POINTS && !moving.contains(&id) {
                    continue;
                }
                changes.add_change(
                    ParamAddress::Opaque(id.into()),
                    2 * k as i32,
                    ((id as usize + k) % 5) as f64 / 8.0,
                );
            }
        }
        changes
    }

    /// Each live queue's `(id, [(offset, value)])` as the plugin reads it.
    fn plugin_view(list: &ComWrapper<ParameterChangesImpl>) -> Vec<(u32, Vec<(i32, f64)>)> {
        use vst3::Steinberg::Vst::{
            IParamValueQueueTrait, IParameterChanges, IParameterChangesTrait,
        };
        let ptr = list.to_com_ptr::<IParameterChanges>().unwrap();
        let n = unsafe { ptr.getParameterCount() };
        (0..n)
            .map(|i| unsafe {
                let q = vst3::ComRef::from_raw(ptr.getParameterData(i)).unwrap();
                let points = (0..q.getPointCount())
                    .map(|p| {
                        let (mut offset, mut value) = (0, 0.0);
                        assert_eq!(q.getPoint(p, &mut offset, &mut value), 0);
                        (offset, value)
                    })
                    .collect();
                (q.getParameterId(), points)
            })
            .collect()
    }

    /// **The host's input list holds the carried pool budget, not
    /// parameters × room**: for 2 000 and 16 384 parameters at 512- and
    /// 4096-frame blocks (a chunk room of `chunk_room`, as activation builds
    /// it), a floor of `FLOOR_POINTS + 1` per queue and `POOLED_QUEUES`
    /// blocks of `room + 1`, under a third of a full room per queue.
    ///
    /// Mutation (run): `with_pool` building every queue at full room
    /// (`with_queues(queues, points)` always) → `queues × (room + 1)` →
    /// fails.
    #[test]
    fn the_input_list_holds_the_pool_budget_not_params_times_room() {
        for params in [2_000usize, 16_384] {
            for frames in [512usize, 4096] {
                let room = tutti_plugin_types::ParameterQueue::chunk_room(frames);
                let list = ParameterChangesImpl::with_pool(params, room);
                let budget = ParameterChanges::carried_budget(params, room);
                assert_eq!(
                    budget,
                    params * (ParameterChanges::FLOOR_POINTS + 1)
                        + ParameterChanges::POOLED_QUEUES * (room + 1)
                );
                assert_eq!(list.point_storage(), budget, "{params} at {frames}");
                assert!(3 * budget < params * (room + 1));
            }
        }
    }

    /// **A chunk the host's pool bounded reaches the plugin whole, carried
    /// values and all.** 2 000 parameters at a 512-frame block, the input
    /// list built as activation builds it. The host's chunk at its budget,
    /// the parameters moving at full room the *last* `POOLED_QUEUES`, then a
    /// skipped block's carried value prepended to every queue, as the bridge
    /// does: every floor queue now carries three points and every moving one
    /// its room and one more. The refill hands every point on, none counted:
    /// the floor queues fit their own room, so the blocks go to the moving
    /// ones however late in the list they are.
    ///
    /// Mutation (run): `with_pool`'s floor without the carried value's point
    /// (`FLOOR_POINTS`, as it first was) → the first floor queues borrow the
    /// blocks for their third point, and the moving parameters at the end
    /// find the pool spent → points counted → fails.
    #[test]
    fn a_chunk_with_carried_values_refills_whole() {
        const PARAMS: u32 = 2_000;
        let room = tutti_plugin_types::ParameterQueue::chunk_room(512);
        let list = ParameterChangesImpl::with_pool(PARAMS as usize, room);
        let mut chunk = pooled_chunk(
            PARAMS,
            room,
            PARAMS - ParameterChanges::POOLED_QUEUES as u32,
        );
        for i in 0..PARAMS as usize {
            // As the bridge's `merge_carried` does: a real slot, nothing
            // grows.
            assert!(chunk.prepend_carried(
                i,
                tutti_plugin_types::ParameterPoint {
                    sample_offset: 0,
                    value: tutti_plugin_types::Normalized::new(0.25),
                },
            ));
        }
        assert_no_alloc::assert_no_alloc(|| list.refill_from_changes(&chunk));
        assert_eq!(list.points_dropped(), 0);
        let view = plugin_view(&list);
        for (q, (id, points)) in chunk.queues.iter().zip(&view) {
            let want: Vec<(i32, f64)> = q
                .points
                .iter()
                .map(|p| (p.sample_offset, p.value.get()))
                .collect();
            assert_eq!(points, &want, "parameter {id}");
        }
    }

    /// **A chunk at the pool budget reaches the plugin per parameter and in
    /// time order, without allocating, block after block; past it each
    /// surplus point gives way to its parameter's latest, counted.** 2 000
    /// parameters at a 512-frame block: the host's pooled chunk (its points
    /// arrived interleaved) refills the pooled input list inside the no-alloc
    /// gate, block after block, with a different 256 parameters moving in
    /// alternate blocks and an empty block between (so every block's moving
    /// parameters need the blocks the last one lent), and the plugin reads
    /// every parameter's points, in order, through `getPoint`. Then one
    /// more parameter carries two points past its floor of
    /// `FLOOR_POINTS + 1` with the list's pool spent: it keeps that floor,
    /// ending on its latest value, and one point is counted.
    ///
    /// Mutations (run), each fails: the refill not borrowing a block (no
    /// lend) → the moving parameters keep their floor, points counted;
    /// `return_blocks` skipped in the refill (or in the refill and
    /// `clear_in_place`) → the other 256 parameters find the pool spent,
    /// points counted; `refill_from_queue` dropping past the room
    /// (`try_push`) rather than keeping the latest value
    /// (`push_or_replace_last`) → the over-budget parameter ends on its
    /// earlier value. Not covered: `return_blocks` skipped in
    /// `clear_in_place` alone, since every refill gives the blocks back
    /// first, so a cleared list is never refilled without it.
    #[test]
    fn a_chunk_at_the_pool_budget_refills_per_parameter_without_allocating() {
        const PARAMS: u32 = 2_000;
        let room = tutti_plugin_types::ParameterQueue::chunk_room(512);
        let list = ParameterChangesImpl::with_pool(PARAMS as usize, room);
        let chunk = pooled_chunk(PARAMS, room, 0);
        let other = pooled_chunk(PARAMS, room, ParameterChanges::POOLED_QUEUES as u32);
        let empty = ParameterChanges::new();
        list.refill_from_changes(&chunk);
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..3 {
                list.refill_from_changes(&other);
                list.refill_from_changes(&chunk);
                list.clear_in_place();
                list.refill_from_changes(&empty);
                list.refill_from_changes(&other);
                list.refill_from_changes(&empty);
                list.refill_from_changes(&chunk);
            }
        });
        assert_eq!(list.points_dropped(), 0);
        let view = plugin_view(&list);
        assert_eq!(view.len(), PARAMS as usize);
        for (q, (id, points)) in chunk.queues.iter().zip(&view) {
            assert_eq!(Some(*id), q.param_id.opaque().map(|p| p.get()));
            let want: Vec<(i32, f64)> = q
                .points
                .iter()
                .map(|p| (p.sample_offset, p.value.get()))
                .collect();
            assert_eq!(points, &want, "parameter {id}");
            assert!(points.windows(2).all(|w| w[0].0 <= w[1].0));
        }

        // A set the host's pool did not bound (built by hand): the last
        // parameter with two points past the list's floor.
        let mut over = ParameterChanges::new();
        for q in &chunk.queues {
            over.add_queue(q.clone());
        }
        let last = over.queues.last_mut().expect("a queue");
        let mut grown =
            tutti_plugin_types::ParameterQueue::with_capacity(last.param_id, last.len() + 2);
        for p in &last.points {
            assert!(grown.points.try_push(*p).is_ok());
        }
        assert!(grown.add_point(200, 0.25));
        assert!(grown.add_point(300, 0.875));
        *last = grown;
        assert_no_alloc::assert_no_alloc(|| list.refill_from_changes(&over));
        assert_eq!(list.points_dropped(), 1);
        let (_, last) = plugin_view(&list).pop().expect("a queue");
        assert_eq!(last.len(), ParameterChanges::FLOOR_POINTS + 1);
        assert_eq!(last.last(), Some(&(300, 0.875)), "ends on its latest");
    }
}
