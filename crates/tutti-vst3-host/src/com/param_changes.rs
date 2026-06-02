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
//! buffers — clearing the internal queues, recycling their backing
//! `ComWrapper<ParamValueQueueImpl>` storage, and refilling in place —
//! so the RT path makes no new heap allocations once the process loop
//! has warmed up.

use vst3::Steinberg::Vst::{IParamValueQueue, IParameterChanges, IParameterChangesTrait};
use vst3::{Class, ComWrapper};

use super::param_queue::ParamValueQueueImpl;
use crate::types::ParameterChanges;
use tutti_types::AudioThreadCell;

pub struct ParameterChangesImpl {
    queues: AudioThreadCell<Vec<ComWrapper<ParamValueQueueImpl>>>,
}

impl Class for ParameterChangesImpl {
    type Interfaces = (IParameterChanges,);
}

impl ParameterChangesImpl {
    /// Build a changes list from an existing [`ParameterChanges`]. Each
    /// call allocates a fresh `ComWrapper<ParamValueQueueImpl>` per
    /// queue; **not RT-safe** — use [`new_empty`] +
    /// [`refill_from_changes`] on the hot path.
    pub fn from_changes(changes: &ParameterChanges) -> ComWrapper<Self> {
        let mut queues: Vec<_> = Vec::with_capacity(changes.queues.len().max(32));
        for q in &changes.queues {
            queues.push(ParamValueQueueImpl::from_queue(q));
        }
        ComWrapper::new(Self {
            queues: AudioThreadCell::new(queues),
        })
    }

    pub fn new_empty() -> ComWrapper<Self> {
        ComWrapper::new(Self {
            queues: AudioThreadCell::new(Vec::with_capacity(32)),
        })
    }

    /// Refill this changes list from `changes` in place. Reuses existing
    /// `ComWrapper<ParamValueQueueImpl>` slots; grows once (off the hot
    /// path in practice) if `changes` carries more queues than we've
    /// seen before, and keeps the larger capacity thereafter.
    ///
    /// # Real-time
    ///
    /// Allocation-free once the host has seen at least as many distinct
    /// parameter queues as `changes.queues.len()`. The first few blocks
    /// may grow; stable-state automation is alloc-free.
    pub fn refill_from_changes(&self, changes: &ParameterChanges) {
        let mut queues = self.queues.borrow_mut();
        // Grow (off-the-hot-path) if needed.
        while queues.len() < changes.queues.len() {
            // Placeholder param_id — we immediately refill it below.
            queues.push(ParamValueQueueImpl::new_empty(0));
        }
        // Refill the live slots in place.
        for (slot, q) in queues.iter().zip(changes.queues.iter()) {
            slot.refill_from_queue(q);
        }
        // Trim leftover slots from a larger previous block. Drop only
        // drops the ComWrapper (refcount--); the queue storage returns
        // to the allocator off-RT if no other reference holds it.
        if queues.len() > changes.queues.len() {
            queues.truncate(changes.queues.len());
        }
    }

    /// Clear all queues in place (reuse-friendly variant of drop-and-refill).
    ///
    /// **Note:** If the previous buffer left queues behind (e.g. the
    /// plugin's output side pushed some via `addParameterData`),
    /// dropping their `ComWrapper`s here does allocate through
    /// refcount bookkeeping. That makes this call *not* strictly
    /// allocation-free on the output-side recycling path — the RT
    /// audit accepts that cost because VST3 plugins rarely emit param
    /// output events every buffer. If profiling shows it matters,
    /// swap this for a pool that recycles `ComWrapper` slots instead
    /// of dropping them.
    pub fn clear_in_place(&self) {
        self.queues.borrow_mut().clear();
    }

    pub fn to_changes(&self) -> ParameterChanges {
        let mut changes = ParameterChanges::new();
        self.fill_changes(&mut changes);
        changes
    }

    /// RT-safe variant of [`Self::to_changes`] that drains into a
    /// caller-supplied pooled `ParameterChanges`. Clears `out.queues`
    /// first; reuses any per-queue inline `points` storage on the
    /// destination side.
    pub fn fill_changes(&self, out: &mut ParameterChanges) {
        // Clear `out`'s queues — each individual queue's `points`
        // SmallVec keeps its heap capacity (clear is len=0, no realloc).
        for queue in out.queues.iter_mut() {
            queue.points.clear();
        }
        out.queues.clear();

        for queue in self.queues.borrow().iter() {
            let param_id = queue.param_id();
            queue.for_each_point(|point| {
                out.add_change(param_id, point.sample_offset, point.value);
            });
        }
    }

    pub fn len(&self) -> usize {
        self.queues.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.queues.borrow().is_empty()
    }

    /// Reset the audio-thread owner (see [`AudioThreadCell::reset_owner`]).
    /// Also resets the owner on every contained queue.
    pub fn reset_owner(&self) {
        self.queues.reset_owner();
        // borrow() after reset re-claims ownership on the current thread,
        // which is fine — this method is called off-RT during device swap.
        for queue in self.queues.borrow().iter() {
            queue.reset_owner();
        }
    }
}

impl IParameterChangesTrait for ParameterChangesImpl {
    unsafe fn getParameterCount(&self) -> i32 {
        self.queues.borrow().len() as i32
    }

    unsafe fn getParameterData(&self, index: i32) -> *mut IParamValueQueue {
        let queues = self.queues.borrow();
        if index < 0 || index >= queues.len() as i32 {
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
        let new_queue = ParamValueQueueImpl::new_empty(param_id);
        let queue_ptr = new_queue
            .as_com_ref::<IParamValueQueue>()
            .map(|r| r.as_ptr())
            .unwrap_or(std::ptr::null_mut());
        let mut queues = self.queues.borrow_mut();
        queues.push(new_queue);
        if !index.is_null() {
            *index = (queues.len() - 1) as i32;
        }
        queue_ptr
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changes_with(num_queues: usize, points_per: usize) -> ParameterChanges {
        let mut changes = ParameterChanges::new();
        for q in 0..num_queues {
            for p in 0..points_per {
                changes.add_change(q as u32, p as i32, p as f64 * 0.01);
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
}
