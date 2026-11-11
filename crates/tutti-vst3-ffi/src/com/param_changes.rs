//! `IParameterChanges`: a pool of preallocated queues the plugin reads (input)
//! or fills through `addParameterData` (output) during one `process` call.
//!
//! The list's state sits in an [`AudioThreadCell`] and each queue's in its own
//! (see [`super::event_list`] and [`super::param_queue`]); the host takes every
//! borrow with `try_` and skips the block, counted, when a plugin thread holds
//! one ([`ParameterChangesImpl::try_begin`]).
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

use crate::guard::{Out, guard};
use vst3::Steinberg::Vst::{IParamValueQueue, IParameterChanges, IParameterChangesTrait};
use vst3::{Class, ComWrapper};

use super::param_queue::ParamValueQueueImpl;
use std::sync::atomic::{AtomicU64, Ordering};
use tutti_plugin_types::{Normalized, ParamId, PointPool};
use tutti_plugin_types::{ParameterChanges, ParameterPoint};

use tutti_rt::{AudioThreadCell, BorrowGuard};

/// The host's `IParameterChanges`: a pool of preallocated queues the plugin
/// reads (input) or fills through `addParameterData` (output) during one
/// `process` call. Nothing allocates after construction.
///
/// The host reaches the list only through [`try_begin`](Self::try_begin)'s
/// [`ParamStaging`] (to stage a block) or
/// [`try_fill_changes`](Self::try_fill_changes) (to read what the plugin
/// wrote); both take `try_` borrows and count a refusal in
/// [`busy`](Self::busy), so a plugin thread still inside the list or one of
/// its queues costs the host a skipped block, never a wait or a panic.
pub struct ParameterChangesImpl {
    /// `live` and the point pool: what a plugin thread's `addParameterData` and
    /// the host's staging both change.
    inner: AudioThreadCell<Inner>,
    /// The pool of queues, built once and never resized, so the counts below
    /// read without a borrow. The first `inner.live` are this block's.
    queues: Box<[ComWrapper<ParamValueQueueImpl>]>,
    /// Points a refill or `addParameterData` could not hand on whole. Atomic, not in
    /// `inner`, so a COM entry that finds `inner` borrowed still counts its refusal.
    dropped: AtomicU64,
    /// Calls the audio thread skipped because a plugin thread held the list or
    /// one of its queues ([`busy`](Self::busy)).
    busy: AtomicU64,
}

struct Inner {
    live: usize,
    /// The shared point pool, lent to `queues` by index (the input list's;
    /// no blocks otherwise).
    pool: PointPool,
}

/// The list or one of its queues is in use by a plugin thread; the block is
/// skipped (counted in [`ParameterChangesImpl::busy`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Busy;

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
            inner: AudioThreadCell::new(Inner { live: 0, pool }),
            // Placeholder param ids: each queue is refilled before it is live.
            queues: (0..queues)
                .map(|_| ParamValueQueueImpl::with_room(ParamId::new(0), floor))
                .collect(),
            dropped: AtomicU64::new(0),
            busy: AtomicU64::new(0),
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
        let added: u64 = self.queues.iter().map(|q| q.points_dropped()).sum();
        self.dropped.load(Ordering::Relaxed) + added
    }

    /// How many calls the host skipped because a plugin thread held the list
    /// or one of its queues ([`try_begin`](Self::try_begin),
    /// [`try_fill_changes`](Self::try_fill_changes)).
    pub fn busy(&self) -> u64 {
        self.busy.load(Ordering::Relaxed)
    }

    /// Counts refused points.
    fn count_dropped(&self, n: u64) {
        self.dropped.fetch_add(n, Ordering::Relaxed);
    }

    /// Counts one call skipped for a busy list or queue.
    fn count_busy(&self) {
        self.busy.fetch_add(1, Ordering::Relaxed);
    }

    /// Every point the list has storage for, in its queues and its pool.
    #[cfg(test)]
    pub(crate) fn point_storage(&self) -> usize {
        let inner = self.inner.borrow();
        let queues: usize = self.queues.iter().map(|q| q.stage().capacity()).sum();
        queues + inner.pool.storage()
    }

    /// Empties the list and opens it for staging a block, or `None` (counted
    /// in [`busy`](Self::busy)) while a plugin thread is inside one of its
    /// calls or holds a queue that lent a block: the audio thread skips the
    /// block rather than wait or panic, and what is left stays for the next
    /// call to clear. The queues stay in the pool, so this frees nothing and
    /// never allocates; the guard is the only way to stage, so staging cannot
    /// reach the storage without the boundary.
    ///
    /// Drop the guard before handing the list to the plugin: it holds the
    /// list's borrow.
    pub fn try_begin(&self) -> Option<ParamStaging<'_>> {
        let Some(mut inner) = self.inner.try_borrow_mut() else {
            self.count_busy();
            return None;
        };
        inner.live = 0;
        if !Self::return_blocks(&self.queues, &mut inner) {
            self.count_busy();
            return None;
        }
        Some(ParamStaging { list: self, inner })
    }

    /// [`try_begin`](Self::try_begin) for a test, which owns the list's every
    /// reader.
    ///
    /// # Panics
    ///
    /// If a plugin thread is inside a call on the list or holds a queue.
    #[cfg(test)]
    pub(crate) fn begin(&self) -> ParamStaging<'_> {
        self.try_begin().expect("the list is not in use")
    }

    /// Takes back every block lent for the last block, giving each queue its
    /// floor storage back ([`PointPool::try_give_back`]). Allocation-free.
    /// `false` when a queue was held by a plugin thread: its block stays
    /// lent for the next call.
    fn return_blocks(queues: &[ComWrapper<ParamValueQueueImpl>], inner: &mut Inner) -> bool {
        let mut all = true;
        inner.pool.try_give_back(|i, own| match queues.get(i) {
            Some(q) => match q.try_stage() {
                Some(mut state) => {
                    state.swap_points(own);
                    Some(true)
                }
                None => {
                    all = false;
                    None
                }
            },
            None => Some(false),
        });
        all
    }

    /// Drains the plugin's emitted parameter changes into a caller-supplied
    /// pooled [`ParameterChanges`]. Clears `out` first with
    /// [`ParameterChanges::clear`], which hands its queues back to its spares
    /// with their point storage kept, so for an `out` built
    /// [`ParameterChanges::reserved`] this never allocates: past a queue's
    /// room with `out`'s pool spent, the queue keeps the parameter's latest
    /// value, counted in [`ParameterChanges::points_overflowed`].
    ///
    /// `false`, counted in [`busy`](Self::busy), while a plugin thread is
    /// inside the list or one of its queues: `out` then holds what was read
    /// before, which the caller discards.
    #[must_use = "a refused read leaves `out` partial"]
    pub fn try_fill_changes(&self, out: &mut ParameterChanges) -> bool {
        out.clear();

        let Some(inner) = self.inner.try_borrow() else {
            self.count_busy();
            return false;
        };
        for queue in self.queues.iter().take(inner.live) {
            let Some(state) = queue.try_stage() else {
                self.count_busy();
                out.clear();
                return false;
            };
            // The plugin answered with a bare `ParamID`, and VST3 ids are opaque: tag it.
            let param_id = state.param_id();
            state.for_each_point(|point| {
                out.add_change(param_id, point.sample_offset, point.value.get());
            });
        }
        true
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
        self.queues.len()
    }

    /// A hold on queue `index` as a plugin thread inside `getPoint` has one,
    /// for a conformance test of what the host does while the plugin holds it.
    #[cfg(any(test, feature = "conformance"))]
    pub fn hold_queue_as_plugin(&self, index: usize) -> impl Sized + '_ {
        self.queues[index].hold_as_plugin()
    }
}

/// A block being staged into a [`ParameterChangesImpl`]; the list is borrowed
/// until drop. Every queue a method touches is taken with `try_` for that
/// call, and a refusal is [`Busy`], counted.
#[must_use = "the list is borrowed until dropped"]
pub struct ParamStaging<'a> {
    list: &'a ParameterChangesImpl,
    inner: BorrowGuard<'a, Inner>,
}

impl ParamStaging<'_> {
    /// Refills the list from `changes` in place, reusing the queues.
    ///
    /// Allocation-free. The queues past the list's are dropped, their points
    /// counted ([`points_dropped`](ParameterChangesImpl::points_dropped)). No queue's points
    /// grow: one that needs more than its own room borrows a block from the
    /// list's pool until the next [`try_begin`](ParameterChangesImpl::try_begin),
    /// and past that the queue keeps the parameter's latest value, counted.
    ///
    /// # Errors
    ///
    /// [`Busy`] if a plugin thread holds one of the queues; the list is left
    /// empty.
    pub fn refill(&mut self, changes: &ParameterChanges) -> Result<(), Busy> {
        let Self { list, inner } = self;
        let Inner { live, pool } = &mut **inner;
        let past = changes.queues.get(list.queues.len()..).unwrap_or_default();
        let mut lost: u64 = past.iter().map(|q| q.points.len() as u64).sum();
        for (i, (slot, q)) in list.queues.iter().zip(changes.queues.iter()).enumerate() {
            let Some(mut state) = slot.try_stage() else {
                *live = 0;
                list.count_dropped(lost);
                list.count_busy();
                return Err(Busy);
            };
            if q.points.len() > state.capacity() {
                // A block for the block, when the pool has one left; the queue's own
                // storage (refilled below anyway) is kept to swap back.
                let _ = state.lend_from(pool, i);
            }
            lost += state.refill(q);
        }
        *live = changes.queues.len() - past.len();
        list.count_dropped(lost);
        Ok(())
    }

    /// Puts a point for `param_id` ahead of the list's own, at the block's
    /// first sample: the host's own write, which the automation already in the
    /// list overrides where both set the parameter at one offset.
    ///
    /// Adds to the parameter's queue when the list has one, otherwise takes
    /// the next unused queue. A queue with no room, or a list with no queue
    /// left, drops the point and counts it
    /// ([`points_dropped`](ParameterChangesImpl::points_dropped)): the automation outranks it.
    /// Allocation-free; linear in the live queues.
    ///
    /// # Errors
    ///
    /// [`Busy`] if a plugin thread holds a queue it needs.
    pub fn prepend(&mut self, param_id: ParamId, value: Normalized) -> Result<(), Busy> {
        let point = ParameterPoint {
            sample_offset: 0,
            value,
        };
        let Self { list, inner } = self;
        let live = inner.live;
        let mut landed = false;
        let mut busy = false;
        'find: {
            for queue in &list.queues[..live] {
                let Some(mut state) = queue.try_stage() else {
                    busy = true;
                    break 'find;
                };
                if state.param_id() == param_id {
                    landed = state.prepend(point);
                    break 'find;
                }
            }
            if let Some(free) = list.queues.get(live) {
                match free.try_stage() {
                    Some(mut state) => {
                        state.reset(param_id);
                        if state.prepend(point) {
                            inner.live = live + 1;
                            landed = true;
                        }
                    }
                    None => busy = true,
                }
            }
        }
        if busy {
            list.count_busy();
            return Err(Busy);
        }
        if !landed {
            list.count_dropped(1);
        }
        Ok(())
    }
}

opaque_debug!(ParamStaging<'_>, "ParamStaging");

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
                .map(|i| &self.queues[i])
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
        // SAFETY: a COM out-parameter: null or valid for this call.
        let index = unsafe { Out::new(index) };
        guard(std::ptr::null_mut(), || {
            if id.is_null() {
                return std::ptr::null_mut();
            }
            // SAFETY: `id` is non-null (checked above) and, by the ABI, valid for
            // a read of one `ParamID` for this call.
            let param_id = ParamId::new(unsafe { *id });
            let Some(mut inner) = self.inner.try_borrow_mut() else {
                self.count_dropped(1);
                return std::ptr::null_mut();
            };
            let live = inner.live;
            let at = match self.queues[..live]
                .iter()
                .position(|q| q.try_param_id() == Some(param_id))
            {
                Some(at) => at,
                None if live < self.queues.len() => {
                    let Some(mut state) = self.queues[live].try_stage() else {
                        self.count_dropped(1);
                        return std::ptr::null_mut();
                    };
                    state.reset(param_id);
                    inner.live = live + 1;
                    live
                }
                None => {
                    self.count_dropped(1);
                    return std::ptr::null_mut();
                }
            };
            index.set(i32::try_from(at).unwrap_or(i32::MAX));
            self.queues[at]
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
