//! `IParameterChanges`: a pool of preallocated queues the plugin reads (input)
//! or fills through `addParameterData` (output) during one `process` call.
//!
//! The state sits in an [`AudioThreadCell`] (see [`super::event_list`]).
//! Queues are built once, one per parameter as the SDK's own host does
//! (`ParameterChanges::setMaxParameters`): a block uses the first `live` of
//! them and the rest stay allocated, so a block with fewer queues than the
//! last frees nothing and the next with more allocates nothing. Past the
//! queues nothing grows: a refill drops the surplus, counted, and the
//! plugin's `addParameterData` returns null.
//!
//! The host's input list pools its points in the chunk's layout
//! (`ParameterChanges::point_layout`): each queue owns a floor of
//! `ParameterChanges::FLOOR_POINTS` points plus one for the value a skipped
//! block carries, and a queue refilled with more borrows a block of the full
//! room from the list's [`PointPool`]. Past a block, or with the pool spent,
//! a queue keeps the parameter's latest value, counted
//! ([`points_dropped`](ParameterChangesImpl::points_dropped)).

use crate::guard::guard;
use vst3::Steinberg::Vst::{IParamValueQueue, IParameterChanges, IParameterChangesTrait};
use vst3::{Class, ComWrapper};

use super::param_queue::ParamValueQueueImpl;
use std::sync::atomic::{AtomicU64, Ordering};
use tutti_plugin_types::{Normalized, PointPool};
use tutti_plugin_types::{ParameterChanges, ParameterPoint};

use tutti_rt::AudioThreadCell;

/// The host's `IParameterChanges`: a pool of preallocated queues the plugin
/// reads (input) or fills through `addParameterData` (output) during one
/// `process` call. Nothing allocates after construction.
pub struct ParameterChangesImpl {
    inner: AudioThreadCell<Inner>,
    /// Points a refill or `addParameterData` could not hand on whole. Atomic, not in
    /// `inner`, so a COM entry that finds `inner` borrowed still counts its refusal.
    dropped: AtomicU64,
}

struct Inner {
    /// The pool of queues; the first `live` are this block's.
    queues: Vec<ComWrapper<ParamValueQueueImpl>>,
    live: usize,
    /// The shared point pool, lent to `queues` by index (the input list's;
    /// no blocks otherwise).
    pool: PointPool,
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
            inner: AudioThreadCell::new(Inner {
                // Placeholder param ids: each queue is refilled before it is live.
                queues: (0..queues)
                    .map(|_| ParamValueQueueImpl::with_room(0, floor))
                    .collect(),
                live: 0,
                pool,
            }),
            dropped: AtomicU64::new(0),
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
        let inner = self.inner.borrow();
        let added: u64 = inner.queues.iter().map(|q| q.points_dropped()).sum();
        self.dropped.load(Ordering::Relaxed) + added
    }

    /// Counts refused points.
    fn count_dropped(&self, n: u64) {
        self.dropped.fetch_add(n, Ordering::Relaxed);
    }

    /// Every point the list has storage for, in its queues and its pool.
    #[cfg(test)]
    pub(crate) fn point_storage(&self) -> usize {
        let inner = self.inner.borrow();
        let queues: usize = inner.queues.iter().map(|q| q.capacity()).sum();
        queues + inner.pool.storage()
    }

    /// Takes back every block lent for the last block, giving each queue its
    /// floor storage back ([`PointPool::give_back`]). Allocation-free.
    fn return_blocks(inner: &mut Inner) {
        let Inner { queues, pool, .. } = inner;
        pool.give_back(|i, own| match queues.get(i) {
            Some(q) => {
                q.swap_points(own);
                true
            }
            None => false,
        });
    }

    /// Refills the list from `changes` in place, reusing the queues.
    ///
    /// Allocation-free. The queues past the list's are dropped, their points
    /// counted ([`points_dropped`](Self::points_dropped)). No queue's points
    /// grow: one that needs more than its own room borrows a block from the
    /// list's pool until the next refill, and past that the queue keeps the
    /// parameter's latest value, counted.
    pub fn refill_from_changes(&self, changes: &ParameterChanges) {
        let mut inner = self.inner.borrow_mut();
        Self::return_blocks(&mut inner);
        let Inner { queues, live, pool } = &mut *inner;
        let past = changes.queues.get(queues.len()..).unwrap_or_default();
        let mut lost: u64 = past.iter().map(|q| q.points.len() as u64).sum();
        for (i, (slot, q)) in queues.iter().zip(changes.queues.iter()).enumerate() {
            if q.points.len() > slot.capacity() {
                // A block for the block, when the pool has one left; the queue's own
                // storage (refilled below anyway) is kept to swap back.
                let _ = slot.lend_from(pool, i);
            }
            lost += slot.refill_from_queue(q);
        }
        *live = changes.queues.len() - past.len();
        drop(inner);
        self.count_dropped(lost);
    }

    /// Puts a point for `param_id` ahead of the list's own, at the block's
    /// first sample: the host's own write, which the automation already in the
    /// list overrides where both set the parameter at one offset.
    ///
    /// Adds to the parameter's queue when the list has one, otherwise takes
    /// the next unused queue. A queue with no room, or a list with no queue
    /// left, drops the point and counts it
    /// ([`points_dropped`](Self::points_dropped)): the automation outranks it.
    /// Allocation-free; linear in the live queues.
    pub fn prepend_point(&self, param_id: u32, value: Normalized) {
        let point = ParameterPoint {
            sample_offset: 0,
            value,
        };
        let mut inner = self.inner.borrow_mut();
        let live = inner.live;
        let landed = match inner.queues[..live]
            .iter()
            .find(|q| q.param_id() == param_id)
        {
            Some(queue) => queue.prepend_point(point),
            None => match inner.queues.get(live) {
                Some(free) if free.try_reset(param_id) && free.prepend_point(point) => {
                    inner.live = live + 1;
                    true
                }
                _ => false,
            },
        };
        drop(inner);
        if !landed {
            self.count_dropped(1);
        }
    }

    /// Empties the list for the next block. The queues stay in the pool, so
    /// this frees nothing and never allocates.
    pub fn clear_in_place(&self) {
        let mut inner = self.inner.borrow_mut();
        Self::return_blocks(&mut inner);
        inner.live = 0;
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

        let inner = self.inner.borrow();
        for queue in inner.queues.iter().take(inner.live) {
            // The plugin answered with a bare `ParamID`, and VST3 ids are opaque: tag it.
            let param_id = queue.param_id().into();
            queue.for_each_point(|point| {
                out.add_change(param_id, point.sample_offset, point.value.get());
            });
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.borrow().live
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many queues the pool holds, live or not.
    #[cfg(test)]
    pub(crate) fn pooled(&self) -> usize {
        self.inner.borrow().queues.len()
    }
}

/// Every entry point takes its borrow with `try_`: a plugin whose worker
/// threads read the list during `process` share the borrow, and one that
/// reads it while another of its threads adds to it gets the method's failure
/// value (zero, null), where a panicking borrow would unwind into the
/// plugin's C++ frame and abort the process.
impl IParameterChangesTrait for ParameterChangesImpl {
    /// Zero while another thread is adding to the list.
    unsafe fn getParameterCount(&self) -> i32 {
        guard(0, || {
            self.inner
                .try_borrow()
                .map_or(0, |inner| i32::try_from(inner.live).unwrap_or(i32::MAX))
        })
    }

    /// Null while another thread is adding to the list.
    unsafe fn getParameterData(&self, index: i32) -> *mut IParamValueQueue {
        guard(std::ptr::null_mut(), || {
            let Some(inner) = self.inner.try_borrow() else {
                return std::ptr::null_mut();
            };
            let Some(queue) = usize::try_from(index)
                .ok()
                .filter(|&i| i < inner.live)
                .map(|i| &inner.queues[i])
            else {
                return std::ptr::null_mut();
            };
            // Borrowed pointer: VST3 plugins do not release queues returned here.
            queue
                .as_com_ref::<IParamValueQueue>()
                .map_or(std::ptr::null_mut(), |r| r.as_ptr())
        })
    }

    /// The queue for `id` this block, as the SDK's own host answers: the
    /// live one already filed under `id`, or the next of the list's queues,
    /// emptied for it. Past the list's queues it returns null, the failure
    /// the SDK's samples check for (`again.cpp`), counted in
    /// [`points_dropped`](ParameterChangesImpl::points_dropped); it never
    /// allocates, as the plugin calls it on the audio thread. It also returns
    /// null, counted when the count is free, while another thread reads or adds
    /// to the list.
    unsafe fn addParameterData(&self, id: *const u32, index: *mut i32) -> *mut IParamValueQueue {
        guard(std::ptr::null_mut(), || {
            if id.is_null() {
                return std::ptr::null_mut();
            }
            // SAFETY: a COM method: the plugin passes pointers valid for this call, per the
            // VST3 ABI.
            let param_id = unsafe { *id };
            let Some(mut inner) = self.inner.try_borrow_mut() else {
                self.count_dropped(1);
                return std::ptr::null_mut();
            };
            let live = inner.live;
            let at = match inner.queues[..live]
                .iter()
                .position(|q| q.try_param_id() == Some(param_id))
            {
                Some(at) => at,
                None if live < inner.queues.len() => {
                    if !inner.queues[live].try_reset(param_id) {
                        self.count_dropped(1);
                        return std::ptr::null_mut();
                    }
                    inner.live = live + 1;
                    live
                }
                None => {
                    self.count_dropped(1);
                    return std::ptr::null_mut();
                }
            };
            if !index.is_null() {
                // SAFETY: non-null (checked), and the ABI passes a valid out-pointer.
                unsafe { *index = i32::try_from(at).unwrap_or(i32::MAX) };
            }
            inner.queues[at]
                .as_com_ref::<IParamValueQueue>()
                .map_or(std::ptr::null_mut(), |r| r.as_ptr())
        })
    }
}

/// Raw `IParameterChanges*` for `ProcessData`, or null if the wrapper can't
/// expose the interface. Used by the realtime `process` path for both the
/// input and output parameter-change slots.
pub(crate) fn param_changes_ptr(
    changes: &ComWrapper<ParameterChangesImpl>,
) -> *mut IParameterChanges {
    changes
        .as_com_ref::<IParameterChanges>()
        .map(|r| r.as_ptr())
        .unwrap_or(std::ptr::null_mut())
}

opaque_debug!(ParameterChangesImpl);

#[cfg(test)]
mod tests;
