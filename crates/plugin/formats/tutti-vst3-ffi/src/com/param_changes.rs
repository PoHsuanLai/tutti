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
//! buffers. Its queues are built once, for every parameter, the way the
//! SDK's own host reserves one queue per parameter
//! (`ParameterChanges::setMaxParameters`): a block uses the first `live` of
//! them and the rest stay allocated, so a block with fewer queues than the
//! last frees nothing and the next block with more allocates nothing. (The
//! host sends changes only, so the queue count varies from block to
//! block.) Past them nothing grows: a refill drops the queues past, counted,
//! and the plugin's `addParameterData` returns null.
//!
//! **The host's input list pools its points** in the layout the host's
//! chunk has (`ParameterChanges::point_layout`): each queue owns a floor of
//! `ParameterChanges::FLOOR_POINTS` points and one more for the value a
//! skipped block carries, and a queue refilled with more borrows a block of
//! the full room (and that one more) from the list's shared [`PointPool`],
//! the same pool type the host's sets use, so the list's memory follows the
//! points a chunk carries rather than the parameter count. Every queue's
//! points are a fixed-capacity [`PointBuf`](tutti_plugin_types::PointBuf):
//! past a block, or with the pool spent, the queue keeps the parameter's
//! latest value, counted
//! ([`points_dropped`](ParameterChangesImpl::points_dropped)); nothing grows.

use crate::guard::guard;
use vst3::Steinberg::Vst::{IParamValueQueue, IParameterChanges, IParameterChangesTrait};
use vst3::{Class, ComWrapper};

use super::param_queue::ParamValueQueueImpl;
use tutti_plugin_types::{Normalized, PointPool};
use tutti_plugin_types::{ParameterChanges, ParameterPoint};
use tutti_types::AudioThreadCell;

/// The host's `IParameterChanges`: a pool of preallocated queues the plugin
/// reads (input) or fills through `addParameterData` (output) during one
/// `process` call. Nothing allocates after construction.
pub struct ParameterChangesImpl {
    /// The pool of queues; the first `live` are this block's.
    queues: AudioThreadCell<Vec<ComWrapper<ParamValueQueueImpl>>>,
    /// How many of `queues` hold this block's changes.
    live: AudioThreadCell<usize>,
    /// The shared point pool, lent to `queues` by index (the input list's;
    /// no blocks otherwise).
    pool: AudioThreadCell<PointPool>,
    /// Points a refill could not hand on whole.
    dropped: AudioThreadCell<u64>,
}

impl Class for ParameterChangesImpl {
    type Interfaces = (IParameterChanges,);
}

impl ParameterChangesImpl {
    /// An empty list of `queues` queues, each owning `floor` points, with
    /// `pool`'s blocks to lend a queue past its floor for a refill
    /// ([`PointPool::default`] for none; its holders are the queues'
    /// indices). Allocates; call it off the audio thread with every queue a
    /// block can carry.
    ///
    /// The host's input list is laid out as the host's chunk is
    /// (`ParameterChanges::point_layout` for the chunk's queues and room,
    /// blocks of `room + 1`). The output list, which the plugin fills
    /// through `addPoint` and so cannot borrow, has no per-chunk figure and
    /// gives every queue [`MIN_POINT_ROOM`](super::MIN_POINT_ROOM) and no
    /// pool.
    pub fn with_queues(queues: usize, floor: usize, pool: PointPool) -> ComWrapper<Self> {
        ComWrapper::new(Self {
            // Placeholder param ids: each queue is refilled before it is live.
            queues: AudioThreadCell::new(
                (0..queues)
                    .map(|_| ParamValueQueueImpl::with_room(0, floor))
                    .collect(),
            ),
            live: AudioThreadCell::new(0),
            pool: AudioThreadCell::new(pool),
            dropped: AudioThreadCell::new(0),
        })
    }

    /// Points the list could not hand on whole since it was built: a
    /// refill's points past a queue's room, or past its own floor with the
    /// pool spent (each point that gave way to its parameter's latest
    /// value), and every point of a queue past the list's; the plugin's
    /// `addPoint` past a queue's room and each `addParameterData` refused
    /// (the output list). Zero while the host (or the plugin) stays within
    /// the room the list was built for.
    pub fn points_dropped(&self) -> u64 {
        let added: u64 = self
            .queues
            .borrow()
            .iter()
            .map(|q| q.points_dropped())
            .sum();
        *self.dropped.borrow() + added
    }

    /// Counts one refused `addParameterData`, from a COM entry point:
    /// skipped (uncounted) if the host is reading the count at that moment.
    fn count_dropped(&self) {
        if let Some(mut dropped) = self.dropped.try_borrow_mut() {
            *dropped += 1;
        }
    }

    /// Every point the list has storage for, in its queues and its pool.
    #[cfg(test)]
    pub(crate) fn point_storage(&self) -> usize {
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
    /// Allocation-free. The queues past the list's are dropped, their points
    /// counted ([`points_dropped`](Self::points_dropped)). No queue's points
    /// ever grow: a queue that needs more than its own room borrows a block
    /// from the list's shared pool until the next refill, and past the block
    /// (or with the pool spent, or with no pool) the queue keeps the
    /// parameter's latest value, counted.
    pub fn refill_from_changes(&self, changes: &ParameterChanges) {
        self.return_blocks();
        let queues = self.queues.borrow();
        let mut pool = self.pool.borrow_mut();
        let past = changes.queues.get(queues.len()..).unwrap_or_default();
        let mut dropped: u64 = past.iter().map(|q| q.points.len() as u64).sum();
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
        *self.live.borrow_mut() = changes.queues.len() - past.len();
    }

    /// Puts a point for `param_id` ahead of the list's own, at the block's
    /// first sample: the host's own write, which the automation already in the
    /// list overrides where both set the parameter at one offset.
    ///
    /// Adds to the parameter's queue when the list has one, otherwise takes
    /// the next unused queue of the pool. A queue with no room, or a list with
    /// no queue left, drops the point and counts it
    /// ([`points_dropped`](Self::points_dropped)): the automation outranks it.
    ///
    /// # Real-time
    ///
    /// Allocation-free; linear in the live queues.
    pub fn prepend_point(&self, param_id: u32, value: Normalized) {
        let point = ParameterPoint {
            sample_offset: 0,
            value,
        };
        let queues = self.queues.borrow();
        let live = *self.live.borrow();
        let landed = match queues.iter().take(live).find(|q| q.param_id() == param_id) {
            Some(queue) => queue.prepend_point(point),
            None => match queues.get(live) {
                Some(free) if free.try_reset(param_id) && free.prepend_point(point) => {
                    *self.live.borrow_mut() = live + 1;
                    true
                }
                _ => false,
            },
        };
        if !landed {
            *self.dropped.borrow_mut() += 1;
        }
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
            let param_id = queue.param_id().into();
            queue.for_each_point(|point| {
                out.add_change(param_id, point.sample_offset, point.value.get());
            });
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        *self.live.borrow()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many queues the pool holds, live or not.
    #[cfg(test)]
    pub(crate) fn pooled(&self) -> usize {
        self.queues.borrow().len()
    }
}

/// Every entry point takes its borrows with `try_`: a plugin whose worker
/// threads read the list during `process` share the borrows, and one that
/// reads it while another of its threads adds to it gets the method's failure
/// value (zero, null), where a panicking borrow would unwind into the
/// plugin's C++ frame and abort the process.
impl IParameterChangesTrait for ParameterChangesImpl {
    /// Zero while another thread is adding to the list.
    unsafe fn getParameterCount(&self) -> i32 {
        guard(0, || {
            self.live
                .try_borrow()
                .map_or(0, |live| i32::try_from(*live).unwrap_or(i32::MAX))
        })
    }

    /// Null while another thread is adding to the list.
    unsafe fn getParameterData(&self, index: i32) -> *mut IParamValueQueue {
        guard(std::ptr::null_mut(), || {
            let (Some(queues), Some(live)) = (self.queues.try_borrow(), self.live.try_borrow())
            else {
                return std::ptr::null_mut();
            };
            if index < 0 || index as usize >= *live {
                return std::ptr::null_mut();
            }
            // Borrowed pointer: VST3 plugins do not release queues returned here.
            match queues[index as usize].as_com_ref::<IParamValueQueue>() {
                Some(r) => r.as_ptr(),
                None => std::ptr::null_mut(),
            }
        })
    }

    /// The queue for `id` this block, as the SDK's own host answers: the
    /// live one already filed under `id`, or the next of the list's queues,
    /// emptied for it. Past the list's queues it returns null, the failure
    /// the SDK's samples check for (`again.cpp`), counted in
    /// [`points_dropped`](ParameterChangesImpl::points_dropped): it never
    /// allocates, as the plugin calls it on the audio thread. It returns null
    /// too, counted when the count is free, while another thread reads or
    /// adds to the list.
    unsafe fn addParameterData(&self, id: *const u32, index: *mut i32) -> *mut IParamValueQueue {
        guard(std::ptr::null_mut(), || {
            if id.is_null() {
                return std::ptr::null_mut();
            }
            // SAFETY: a COM method: the plugin passes pointers valid for this call, per the VST3 ABI.
            let param_id = unsafe { *id };
            let (Some(queues), Some(mut live)) =
                (self.queues.try_borrow(), self.live.try_borrow_mut())
            else {
                self.count_dropped();
                return std::ptr::null_mut();
            };
            let at = match queues[..*live]
                .iter()
                .position(|q| q.try_param_id() == Some(param_id))
            {
                Some(at) => at,
                None if *live < queues.len() => {
                    if !queues[*live].try_reset(param_id) {
                        self.count_dropped();
                        return std::ptr::null_mut();
                    }
                    *live += 1;
                    *live - 1
                }
                None => {
                    self.count_dropped();
                    return std::ptr::null_mut();
                }
            };
            if !index.is_null() {
                // SAFETY: `index` is non-null (just checked) and the ABI passes it
                // as a valid out-pointer for the call.
                unsafe { *index = i32::try_from(at).unwrap_or(i32::MAX) };
            }
            queues[at]
                .as_com_ref::<IParamValueQueue>()
                .map(|r| r.as_ptr())
                .unwrap_or(std::ptr::null_mut())
        })
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

impl std::fmt::Debug for ParameterChangesImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParameterChangesImpl")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
