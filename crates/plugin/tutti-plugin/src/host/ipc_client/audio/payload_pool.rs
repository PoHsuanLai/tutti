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
use crossbeam::queue::ArrayQueue;
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

/// Bounded pool of recycled `ProcessPayload` boxes. Cheap to clone (one Arc).
#[derive(Clone)]
pub(super) struct PayloadPool {
    recycle: Arc<ArrayQueue<Box<ProcessPayload>>>,
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
