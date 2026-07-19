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

const POOL_CAPACITY: usize = 4;

/// Bounded pool of recycled `ProcessPayload` boxes. Cheap to clone (one Arc).
#[derive(Clone)]
pub(super) struct PayloadPool {
    recycle: Arc<ArrayQueue<Box<ProcessPayload>>>,
}

impl PayloadPool {
    pub(super) fn new() -> Self {
        Self {
            recycle: Arc::new(ArrayQueue::new(POOL_CAPACITY)),
        }
    }

    /// Audio-thread: hand back a payload to fill in. Falls back to allocation
    /// on cold start; thereafter the pool steady-states with no allocs.
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
