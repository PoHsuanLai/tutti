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

use vst3::Steinberg::Vst::{IParamValueQueue, IParameterChanges, IParameterChangesTrait};
use vst3::{Class, ComWrapper};

use super::param_queue::ParamValueQueueImpl;
use crate::types::ParameterChanges;
use tutti_plugin_types::ParamAddress;
use tutti_types::AudioThreadCell;

pub struct ParameterChangesImpl {
    /// The pool of queues; the first `live` are this block's.
    queues: AudioThreadCell<Vec<ComWrapper<ParamValueQueueImpl>>>,
    /// How many of `queues` hold this block's changes.
    live: AudioThreadCell<usize>,
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
    /// the audio thread with the plugin's parameter count and, for the
    /// host's input list, the points a chunk carries for one parameter at
    /// the activated block size (`ParameterQueue::chunk_room`). The output
    /// list, which the plugin fills, has no such figure and passes
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
        })
    }

    /// Refill this changes list from `changes` in place, reusing the pool's
    /// queues.
    ///
    /// # Real-time
    ///
    /// Allocation-free when the pool holds at least `changes.queues.len()`
    /// queues (and each queue's points fit what it already holds). Past the
    /// pool it grows once and keeps the larger size; it never shrinks.
    pub fn refill_from_changes(&self, changes: &ParameterChanges) {
        let mut queues = self.queues.borrow_mut();
        while queues.len() < changes.queues.len() {
            // Placeholder param_id — refilled immediately below.
            queues.push(ParamValueQueueImpl::new_empty(0));
        }
        for (slot, q) in queues.iter().zip(changes.queues.iter()) {
            slot.refill_from_queue(q);
        }
        *self.live.borrow_mut() = changes.queues.len();
    }

    /// Empties the list for the next block. The queues stay in the pool, so
    /// this frees nothing and never allocates.
    pub fn clear_in_place(&self) {
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
}
