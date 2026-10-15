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
//! ([`with_pool`](ParameterChangesImpl::with_pool)), as the host's chunk
//! does (`ParameterChanges::reserved`): each queue owns
//! `ParameterChanges::FLOOR_POINTS` points, and a queue refilled with more
//! borrows a block of the full room from the list's shared pool for the
//! block, so the list's memory follows the points a chunk carries rather
//! than the parameter count. Past a block, or with the pool spent, the
//! queue's last point gives way to the parameter's latest value, counted
//! ([`points_dropped`](ParameterChangesImpl::points_dropped)); nothing grows.

use vst3::Steinberg::Vst::{IParamValueQueue, IParameterChanges, IParameterChangesTrait};
use vst3::{Class, ComWrapper};

use super::param_queue::ParamValueQueueImpl;
use crate::types::{ParameterChanges, ParameterPoint};
use tutti_plugin_types::ParamAddress;
use tutti_types::AudioThreadCell;

pub struct ParameterChangesImpl {
    /// The pool of queues; the first `live` are this block's.
    queues: AudioThreadCell<Vec<ComWrapper<ParamValueQueueImpl>>>,
    /// How many of `queues` hold this block's changes.
    live: AudioThreadCell<usize>,
    /// The points pool's free blocks, each of `block_room` points (a
    /// [`with_pool`](Self::with_pool) list; empty otherwise).
    blocks: AudioThreadCell<Vec<Vec<ParameterPoint>>>,
    /// Each lent block's queue, and the floor storage it swapped out:
    /// reserved for every block, so a loan never grows it.
    lent: AudioThreadCell<Vec<(usize, Vec<ParameterPoint>)>>,
    /// The points a block holds, or 0 for a list with no pool.
    block_room: usize,
    /// Points a pooled list's refill could not hand on whole.
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
            blocks: AudioThreadCell::new(Vec::new()),
            lent: AudioThreadCell::new(Vec::new()),
            block_room: 0,
            dropped: AudioThreadCell::new(0),
        })
    }

    /// The host's input list: `queues` queues of up to `points` points each
    /// (the points a chunk carries for one parameter at the activated block
    /// size), stored as the host's chunk stores them
    /// ([`ParameterChanges::point_budget`]): each queue owns
    /// [`ParameterChanges::FLOOR_POINTS`], and any
    /// [`ParameterChanges::POOLED_QUEUES`] of them at once borrow a block of
    /// `points` from the list's shared pool. With few enough queues every
    /// queue has the full room instead, as [`with_queues`](Self::with_queues)
    /// gives it. A refill never allocates within that room. Allocates; call
    /// it off the audio thread.
    pub fn with_pool(queues: usize, points: usize) -> ComWrapper<Self> {
        let (floor, blocks) = ParameterChanges::point_layout(queues, points);
        if blocks == 0 {
            return Self::with_queues(queues, points);
        }
        let mut pool = Vec::with_capacity(blocks);
        pool.extend((0..blocks).map(|_| Vec::with_capacity(points)));
        ComWrapper::new(Self {
            queues: AudioThreadCell::new(
                (0..queues)
                    .map(|_| ParamValueQueueImpl::with_exact_room(0, floor))
                    .collect(),
            ),
            live: AudioThreadCell::new(0),
            blocks: AudioThreadCell::new(pool),
            lent: AudioThreadCell::new(Vec::with_capacity(blocks)),
            block_room: points,
            dropped: AudioThreadCell::new(0),
        })
    }

    /// Points a pooled list's refills could not hand on whole, since it was
    /// built: past a queue's room, or past its own floor with the pool
    /// spent, each point that gave way to its parameter's latest value.
    /// Zero while the host stays within the room the list was built for.
    pub fn points_dropped(&self) -> u64 {
        *self.dropped.borrow()
    }

    /// Every point the list has storage for, in its queues and its pool.
    #[cfg(test)]
    pub fn point_storage(&self) -> usize {
        let queues: usize = self.queues.borrow().iter().map(|q| q.capacity()).sum();
        let pool: usize = self.blocks.borrow().iter().map(Vec::capacity).sum();
        let lent: usize = self.lent.borrow().iter().map(|(_, f)| f.capacity()).sum();
        queues + pool + lent
    }

    /// Takes back every block lent for the last block, giving each queue its
    /// floor storage back. Allocation-free.
    fn return_blocks(&self) {
        let queues = self.queues.borrow();
        let mut blocks = self.blocks.borrow_mut();
        for (i, mut floor) in self.lent.borrow_mut().drain(..) {
            queues[i].swap_points(&mut floor);
            // Was in `blocks` before it was lent: the push never grows it.
            blocks.push(floor);
        }
    }

    /// Refill this changes list from `changes` in place, reusing the pool's
    /// queues.
    ///
    /// # Real-time
    ///
    /// Allocation-free when the pool holds at least `changes.queues.len()`
    /// queues (and each queue's points fit what it already holds). Past the
    /// pool it grows once and keeps the larger size; it never shrinks.
    ///
    /// A [`with_pool`](Self::with_pool) list never grows a queue's points: a
    /// queue that needs more than its floor borrows a block from the shared
    /// pool until the next refill, and past the block (or with the pool
    /// spent) its last point gives way to the parameter's latest value,
    /// counted ([`points_dropped`](Self::points_dropped)).
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
        if self.block_room == 0 {
            for (slot, q) in queues.iter().zip(changes.queues.iter()) {
                slot.refill_from_queue(q);
            }
        } else {
            let mut blocks = self.blocks.borrow_mut();
            let mut lent = self.lent.borrow_mut();
            let mut dropped = 0;
            for (i, (slot, q)) in queues.iter().zip(changes.queues.iter()).enumerate() {
                let room = slot.capacity();
                if q.points.len() > room && room < self.block_room {
                    if let Some(mut block) = blocks.pop() {
                        // The block is empty; the queue's floor (refilled
                        // below anyway) is kept to swap back.
                        slot.swap_points(&mut block);
                        block.clear();
                        lent.push((i, block));
                    }
                }
                dropped += slot.refill_within(q);
            }
            *self.dropped.borrow_mut() += dropped;
        }
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
    /// [`ParameterChanges::reserved`] this is allocation-free after warmup.
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
        let mut changes = ParameterChanges::new();
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
    /// Mutation (run): `with_room` ignoring `points` (`Vec::new()`) →
    /// the first refill grows each queue inside the gate → fails.
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
        let mut out = ParameterChanges::new();
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

    /// The host's chunk at the pool budget: `params` parameters with a room
    /// of `room`, the first `POOLED_QUEUES` at full room and the rest at
    /// their floor, the points arriving interleaved (frame by frame, every
    /// parameter) into a pooled set, as the host's node and the wire's
    /// deserialize hand it on. The parameters moving at full room are
    /// `first..first + POOLED_QUEUES`. Point `k` of parameter `id` is on
    /// frame `2k` with value `(id + k) % 5 / 8`.
    fn pooled_chunk(params: u32, room: usize, first: u32) -> ParameterChanges {
        let mut changes = ParameterChanges::reserved(params as usize, room);
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

    /// **The host's input list holds the pool budget, not parameters ×
    /// room**: for 2 000 and 16 384 parameters at 512- and 4096-frame blocks
    /// (a room of `chunk_room + 1`, as activation builds it), a floor per
    /// queue and `POOLED_QUEUES` blocks of the room, under a quarter of a
    /// full room per queue.
    ///
    /// Mutation (run): `with_pool` building every queue at full room
    /// (`with_queues(queues, points)` always) → `queues × room` → fails.
    #[test]
    fn the_input_list_holds_the_pool_budget_not_params_times_room() {
        for params in [2_000usize, 16_384] {
            for frames in [512usize, 4096] {
                let room = tutti_plugin_types::ParameterQueue::chunk_room(frames) + 1;
                let list = ParameterChangesImpl::with_pool(params, room);
                let budget = ParameterChanges::point_budget(params, room);
                assert_eq!(
                    budget,
                    params * ParameterChanges::FLOOR_POINTS
                        + ParameterChanges::POOLED_QUEUES * room
                );
                assert_eq!(list.point_storage(), budget, "{params} at {frames}");
                assert!(4 * budget < params * room);
            }
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
    /// more parameter carries a third point with the list's pool spent: it
    /// keeps its floor, ending on its latest value, and one point is
    /// counted.
    ///
    /// Mutations (run), each fails: the refill not borrowing a block (no
    /// lend) → the moving parameters keep their floor, points counted;
    /// `return_blocks` skipped in the refill (or in the refill and
    /// `clear_in_place`) → the other 256 parameters find the pool spent,
    /// points counted; `refill_within` keeping the room's last point rather
    /// than the queue's last (no `last`) → the over-budget parameter ends on
    /// its earlier value. Not covered: `return_blocks` skipped in
    /// `clear_in_place` alone, since every refill gives the blocks back
    /// first, so a cleared list is never refilled without it.
    #[test]
    fn a_chunk_at_the_pool_budget_refills_per_parameter_without_allocating() {
        const PARAMS: u32 = 2_000;
        let room = tutti_plugin_types::ParameterQueue::chunk_room(512) + 1;
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

        let mut over = chunk.clone();
        over.add_change(ParamAddress::Opaque((PARAMS - 1).into()), 300, 0.875);
        assert_no_alloc::assert_no_alloc(|| list.refill_from_changes(&over));
        assert_eq!(list.points_dropped(), 1);
        let (_, last) = plugin_view(&list).pop().expect("a queue");
        assert_eq!(last.len(), ParameterChanges::FLOOR_POINTS);
        assert_eq!(last.last(), Some(&(300, 0.875)), "ends on its latest");
    }
}
