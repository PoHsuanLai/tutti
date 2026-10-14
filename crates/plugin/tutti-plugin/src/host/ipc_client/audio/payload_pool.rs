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
pub(super) const PARAM_BUFFERS: usize = 8;

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
            params: Arc::new(ArrayQueue::new(PARAM_BUFFERS)),
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
            // Cannot fail: drained above, and only `return_params` pushes,
            // which refuses a buffer reserved before this call.
            let _ = self.params.push(ParameterChanges::reserved(queues, points));
        }
    }

    /// Audio thread: a free parameter-change buffer, or `None` when every one
    /// is in flight. Allocation-free.
    #[inline]
    pub(super) fn take_params(&self) -> Option<ParameterChanges> {
        self.params.pop()
    }

    /// Bridge thread (or the audio thread handing back one it could not
    /// send): return a parameter-change buffer, cleared, to the free list.
    /// One smaller than the current reservation is dropped: the empty
    /// placeholder a payload carries when its chunk had no changes, or a
    /// buffer reserved before a larger reservation. Before any reservation
    /// nothing is kept. Allocation-free on the audio thread:
    /// that caller only ever returns a buffer it took from here, which has
    /// the reserved capacity, and a full list is impossible for it.
    #[inline]
    pub(super) fn return_params(&self, mut changes: ParameterChanges) {
        let room = self.params_room.load(Ordering::Acquire);
        if room == 0 || changes.capacity() < room {
            return;
        }
        changes.clear();
        let _ = self.params.push(changes);
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
