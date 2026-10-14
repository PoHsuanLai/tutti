//! RT-safe `ProcessPayload` recycling.
//!
//! A small bounded queue of pre-built payloads. The audio thread
//! [`acquire`](PayloadPool::acquire)s one per `Process`; the bridge thread
//! [`recycle`](PayloadPool::recycle)s it after marshalling the wire message.
//! When the queue is empty (cold start, brief stalls) `acquire` falls back
//! to a fresh `Box`; in steady state the pool keeps the audio path
//! allocation-free.
//!
//! A named primitive whose method names document the RT contract instead
//! of leaving it implicit in `unwrap_or_else(|| Box::new(...))`.

use super::messages::ProcessPayload;
use crate::protocol::ParameterChanges;
use crossbeam::queue::ArrayQueue;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// One payload per command the audio thread can have queued.
///
/// This is the SAME constraint as the command-queue depth, not an independent
/// tunable. A payload is recycled only once the bridge thread dequeues its
/// command, so the audio thread can hold up to `COMMAND_QUEUE_SIZE` of them at
/// once — which is what happens whenever the bridge thread falls behind, the
/// very case this pipeline exists to survive. Sizing the pool below that depth
/// does not bound memory: the payloads still exist, they are just allocated on
/// the audio thread instead, one ~9 KiB `Box::new` per block, exactly when the
/// plugin is already struggling.
///
/// It was 4 against a queue of 128, so blocks 5..=128 each allocated.
const POOL_CAPACITY: usize = super::channels::COMMAND_QUEUE_SIZE;

/// Parameter-change buffers in circulation between the audio thread and the
/// bridge thread: how many chunks carrying parameter changes can be on their
/// way to the plugin-server at once.
///
/// **Why a pool of its own, and why this small.** A chunk's parameter changes
/// are a [`ParameterChanges`] with room for a queue per parameter of the
/// plugin, reserved at prepare ([`PayloadPool::reserve_params`]). Carrying one
/// in each of the [`POOL_CAPACITY`] payloads would reserve that per payload:
/// 128 copies of a 2 000-parameter table is tens of megabytes per plugin,
/// almost all of it idle, because a payload carries changes only when a
/// parameter moved. So the buffers circulate separately: the audio thread
/// swaps a full one out for a free one when it submits a chunk with changes,
/// and the bridge thread hands it back once the chunk is on the wire.
///
/// Running out means this many chunks with changes are queued and the
/// bridge thread has sent none of them: the plugin-server is that far
/// behind. The plugin node then keeps the chunk's changes and sends each
/// parameter's latest value on the next chunk's first frame (late, never
/// lost), and counts the chunk
/// ([`ParamDeliveryStats::chunks_deferred`](crate::handles::ParamDeliveryStats)).
///
/// **What it costs.** Each buffer holds `queues` parameter queues (the
/// plugin's parameter count plus the node's 64 spare slots), each with room
/// for `points_for(chunk)` points (`ceil(chunk / 32) + 4`, at least 10) of
/// 16 bytes, on top of the queue itself (about 180 bytes with its inline
/// room): about 2.3 KB a queue at a 4096-frame chunk. With the node's own
/// buffer that is `(PARAM_BUFFERS + 1) × queues × (180 + 16 × points)`
/// bytes per plugin: about 43 MB for 2 000 parameters at a 4096-frame chunk,
/// about 340 MB for a VST3 listing 16 384. Doc 014 §12 records the
/// follow-up (a shared per-chunk point arena, or a small per-queue room and
/// a counted shared overflow).
pub(super) const PARAM_BUFFERS: usize = 8;

/// The free list's capacity: twice the buffers in circulation, so a buffer
/// the audio thread hands back always fits (it never drops one, which would
/// free on that thread), while the bridge thread drops what comes back once
/// [`PARAM_BUFFERS`] are free.
const PARAM_SLOTS: usize = 2 * PARAM_BUFFERS;

/// Bounded pool of recycled `ProcessPayload` boxes, and the parameter-change
/// buffers that travel in them. Cheap to clone (one Arc each).
#[derive(Clone)]
pub(super) struct PayloadPool {
    recycle: Arc<ArrayQueue<Box<ProcessPayload>>>,
    /// Free parameter-change buffers, each with room for `params_room`
    /// queues. Empty until [`reserve_params`](Self::reserve_params).
    params: Arc<ArrayQueue<ParameterChanges>>,
    /// The queue capacity the buffers were reserved with: a buffer coming back
    /// with less (one reserved before a larger re-reserve) is dropped rather
    /// than recycled, on the bridge thread.
    params_room: Arc<AtomicUsize>,
}

impl PayloadPool {
    /// Builds the pool **full**, on the control thread.
    ///
    /// Filling here rather than letting it warm up through `recycle` is the
    /// point: an empty pool pushes its allocations onto the audio thread, and
    /// the first blocks after a plugin loads are the least forgiving moment for
    /// one. This runs once, off the RT path, where a few hundred KiB of boxes
    /// costs nothing.
    pub(super) fn new() -> Self {
        let recycle = ArrayQueue::new(POOL_CAPACITY);
        for _ in 0..POOL_CAPACITY {
            // Cannot fail: the queue was just created with this capacity.
            let _ = recycle.push(Box::new(ProcessPayload::empty()));
        }
        Self {
            recycle: Arc::new(recycle),
            params: Arc::new(ArrayQueue::new(PARAM_SLOTS)),
            params_room: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Fill the parameter-change buffers, each with room for `queues` queues
    /// (the plugin's parameter count, and some) of `points` points each (the
    /// node's per-chunk point budget, `ParameterChanges::reserved`). Control
    /// thread: allocates. Buffers reserved before are dropped here, or on
    /// the bridge thread when one with fewer queues comes back. (One with
    /// fewer points per queue, in flight across a re-prepare, is reused: the
    /// node then replaces a full queue's last point, counted, never grows it.)
    pub(super) fn reserve_params(&self, queues: usize, points: usize) {
        self.params_room.store(queues, Ordering::Release);
        while self.params.pop().is_some() {}
        for _ in 0..PARAM_BUFFERS {
            // Cannot fail: drained above, and the list holds twice this many
            // (a buffer the bridge thread returns meanwhile still fits).
            let _ = self.params.push(ParameterChanges::reserved(queues, points));
        }
    }

    /// Audio thread: a free parameter-change buffer, or `None` when every one
    /// is in flight. Allocation-free.
    #[inline]
    pub(super) fn take_params(&self) -> Option<ParameterChanges> {
        self.params.pop()
    }

    /// Bridge thread: return a parameter-change buffer, cleared, to the free
    /// list, once its contents are on the wire. **This is where a buffer is
    /// freed**, never on the audio thread: one smaller than the current
    /// reservation (the empty placeholder a payload carries when its chunk
    /// had no changes, or a buffer reserved before a larger reservation), any
    /// before a reservation, and any past [`PARAM_BUFFERS`] free ones (a
    /// re-reservation refilled the list while older buffers were in flight)
    /// are dropped here.
    #[inline]
    pub(super) fn return_params(&self, mut changes: ParameterChanges) {
        let room = self.params_room.load(Ordering::Acquire);
        if room == 0 || changes.capacity() < room || self.params.len() >= PARAM_BUFFERS {
            return;
        }
        changes.clear();
        let _ = self.params.push(changes);
    }

    /// Audio thread: hand back a buffer taken with
    /// [`take_params`](Self::take_params) that its block could not use (the
    /// command queue refused the block). **Never frees**: it is kept whatever
    /// its capacity (one reserved before a re-reservation goes round once
    /// more and is dropped by the bridge thread on its way back), and the
    /// list has room for it, being twice the buffers the bridge thread keeps.
    /// Only the empty placeholder, which owns no memory, is not kept.
    /// Allocation-free.
    #[inline]
    pub(super) fn return_params_rt(&self, mut changes: ParameterChanges) {
        if changes.capacity() == 0 {
            return;
        }
        changes.clear();
        if let Err(changes) = self.params.push(changes) {
            // Unreachable: the bridge thread keeps at most `PARAM_BUFFERS`
            // free, and the audio thread returns only what it took.
            // Forgotten rather than freed on this thread.
            core::mem::forget(changes);
        }
    }

    /// Audio-thread: hand back a payload to fill in.
    ///
    /// The fallback allocates, which on this thread is a defect rather than a
    /// slow path — it is kept only because returning `Option` would push the
    /// same decision onto every caller, and dropping a block is worse than a
    /// rare malloc. The pool is sized and pre-filled so the fallback is
    /// unreachable in practice: it needs more than [`POOL_CAPACITY`] payloads
    /// outstanding, and the command queue cannot hold that many.
    #[inline]
    pub(super) fn acquire(&self) -> Box<ProcessPayload> {
        self.recycle
            .pop()
            .unwrap_or_else(|| Box::new(ProcessPayload::empty()))
    }

    /// Bridge-thread: return a payload after the wire message has been built.
    /// Drops silently if the pool is full.
    #[inline]
    pub(super) fn recycle(&self, payload: Box<ProcessPayload>) {
        let _ = self.recycle.push(payload);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The audio thread never drops a parameter-change buffer; the bridge
    /// thread drops the surplus.** With every buffer free and one more
    /// coming back (a re-reservation refilled the list while an older one
    /// was in flight), the bridge thread's return drops it; a buffer the
    /// audio thread took and hands back is kept even then, so its memory is
    /// never freed on the audio thread. (A drop is observed as the free
    /// list's length: an assert-no-alloc gate needs the test binary's global
    /// allocator, which this unit-test binary does not install.)
    ///
    /// Mutations (run), each fails: the bridge return without the
    /// `len() >= PARAM_BUFFERS` check → the surplus is kept, the list holds
    /// one more; `return_params_rt` applying the bridge's rule (its body
    /// `self.return_params(changes)`) → the taken buffer is dropped, the
    /// list one short.
    #[test]
    fn the_bridge_drops_surplus_buffers_and_the_audio_thread_none() {
        let pool = PayloadPool::new();
        pool.reserve_params(4, 10);
        assert_eq!(pool.params.len(), PARAM_BUFFERS);

        // A surplus buffer reaches the bridge thread: dropped there.
        pool.return_params(ParameterChanges::reserved(4, 10));
        assert_eq!(
            pool.params.len(),
            PARAM_BUFFERS,
            "the bridge drops the surplus"
        );

        // The audio thread takes one; the bridge returns another meanwhile.
        let taken = pool.take_params().expect("a free buffer");
        pool.return_params(ParameterChanges::reserved(4, 10));
        assert_eq!(pool.params.len(), PARAM_BUFFERS);
        // The block is refused: the audio thread hands its buffer back.
        pool.return_params_rt(taken);
        assert_eq!(
            pool.params.len(),
            PARAM_BUFFERS + 1,
            "the audio thread's buffer is kept, never dropped"
        );
    }
}
