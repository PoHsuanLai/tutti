//! The analysis tap — a lock-free copy of the master output for off-thread
//! consumers (spectrum, pitch, transients).
//!
//! Opt-in: until someone calls [`AudioTap::open`], the audio thread's
//! [`push`](AudioTap::push) is a single atomic load and a return.

use crate::{AtomicBool, Ordering};
use parking_lot::Mutex;
use ringbuf::{
    traits::{Producer, Split},
    HeapCons, HeapProd, HeapRb,
};
use std::sync::Arc;

/// Ring capacity in stereo frames — ~3 seconds at 44.1 kHz.
const CAPACITY: usize = 131_072;

/// The producer end, swapped in on `open` and out on `close`. `Mutex` rather
/// than an atomic swap because only the cold path ever writes it; the audio
/// thread `try_lock`s and skips the buffer on contention.
type TapProducer = Arc<Mutex<Option<HeapProd<(f32, f32)>>>>;

/// Producer half of the analysis tap. Cheap to clone; the audio callback keeps
/// one and pushes every buffer through it.
#[derive(Clone, Default)]
pub struct AudioTap {
    on: Arc<AtomicBool>,
    producer: TapProducer,
}

impl AudioTap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open the tap, returning the consumer end. The caller owns it and drains
    /// it from its own thread. Opening again replaces the ring, orphaning any
    /// previous consumer.
    pub fn open(&self) -> HeapCons<(f32, f32)> {
        let (prod, cons) = HeapRb::<(f32, f32)>::new(CAPACITY).split();
        *self.producer.lock() = Some(prod);
        self.on.store(true, Ordering::Release);
        cons
    }

    /// Close the tap and drop the producer. The consumer sees an empty ring.
    pub fn close(&self) {
        self.on.store(false, Ordering::Release);
        *self.producer.lock() = None;
    }

    pub fn is_open(&self) -> bool {
        self.on.load(Ordering::Acquire)
    }

    /// Push interleaved stereo samples into the ring (RT-safe).
    ///
    /// Called from the audio callback. Never blocks: drops samples if the ring
    /// is full or the producer is mid-swap. No-op while the tap is closed.
    #[inline]
    pub fn push(&self, output: &[f32], frames: usize) {
        if !self.on.load(Ordering::Acquire) {
            return;
        }
        // try_lock: skip this callback if the producer is being swapped.
        if let Some(ref mut guard) = self.producer.try_lock() {
            if let Some(ref mut prod) = **guard {
                output.chunks_exact(2).take(frames).for_each(|ch| {
                    let _ = prod.try_push((ch[0], ch[1]));
                });
            }
        }
    }
}
