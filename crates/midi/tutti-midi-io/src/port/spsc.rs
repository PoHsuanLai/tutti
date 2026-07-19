//! [`SpscRing`] — a lock-free single-producer / single-consumer ring whose
//! producer and consumer halves are touched from *different* threads at the
//! same time (the producer from a midir or audio callback, the consumer from
//! the engine cycle).
//!
//! This is the one concurrency pattern [`AudioThreadCell`](tutti_core::AudioThreadCell)
//! deliberately cannot express: its contract is "at most one borrow at any
//! moment across all threads", whereas an SPSC ring's whole point is a
//! concurrent push and pop. So the `unsafe` that turns the shared `&self` into
//! the `&mut` each ringbuf half needs lives here, encapsulated once, behind a
//! safe API — call sites push/pop with no bare `unsafe`.
//!
//! # Safety contract
//!
//! The soundness rests on the SPSC invariant, which the *caller* upholds:
//! - At most one thread calls [`SpscProducer::push`] (the single producer).
//! - At most one thread calls [`SpscRing::drain_each`] (the single consumer).
//!
//! The underlying `ringbuf` halves are themselves lock-free and tolerate a
//! concurrent producer and consumer; what they cannot tolerate is *two*
//! producers or *two* consumers. The handle types below are `Send`/`Sync` so
//! they can move to the producer thread, but cloning a producer and pushing
//! from two threads would violate the invariant (UB), exactly as it would with
//! the raw `ringbuf` API.

use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use std::cell::UnsafeCell;

/// The consumer-owning half of a lock-free SPSC ring.
///
/// Holds both halves: the consumer is drained through `&self` here (engine
/// thread); the producer is hidden behind [`UnsafeCell`] and only ever reached
/// through the raw pointer a [`SpscProducer`] carries to its own thread.
pub struct SpscRing<T> {
    producer: UnsafeCell<HeapProd<T>>,
    consumer: UnsafeCell<HeapCons<T>>,
}

// SAFETY: the producer is only ever touched via the `*mut` a `SpscProducer`
// carries (single producer thread); the consumer is only ever touched via
// `pop`/`drain_into` on the owning thread (single consumer). Never two of
// either concurrently — the SPSC invariant the caller upholds.
unsafe impl<T: Send> Sync for SpscRing<T> {}

impl<T> SpscRing<T> {
    /// Allocate a ring of `capacity` slots.
    pub fn new(capacity: usize) -> Self {
        let (producer, consumer) = HeapRb::<T>::new(capacity).split();
        Self {
            producer: UnsafeCell::new(producer),
            consumer: UnsafeCell::new(consumer),
        }
    }

    /// Hand out the producer half as a `Send` handle for the producer thread.
    ///
    /// Call once per ring: every returned handle aliases the same producer, so
    /// pushing from two of them concurrently breaks the single-producer
    /// invariant.
    pub fn producer(&self) -> SpscProducer<T> {
        SpscProducer {
            producer: self.producer.get(),
        }
    }

    /// Drain every available item through `f` (consumer side).
    #[inline]
    pub fn drain_each(&self, mut f: impl FnMut(T)) {
        // SAFETY: single consumer — `drain_each` is the only consumer-side
        // access and is called from one thread at a time (the SPSC invariant).
        let consumer = unsafe { &mut *self.consumer.get() };
        while let Some(item) = consumer.try_pop() {
            f(item);
        }
    }
}

/// The producer half of a [`SpscRing`], movable to the producer thread.
///
/// `push` is the single producer-side entry point; the SPSC invariant requires
/// exactly one thread to use a given producer (do not clone-and-push from two).
pub struct SpscProducer<T> {
    producer: *mut HeapProd<T>,
}

// SAFETY: `HeapProd<T>` is `Send` when `T: Send`; ownership of the producer is
// logically transferred to whichever thread holds this handle.
unsafe impl<T: Send> Send for SpscProducer<T> {}
// SAFETY: a single producer thread uses `push`; declaring `Sync` only lets the
// handle sit behind shared references — it does not sanction two concurrent
// pushers (that would violate the SPSC invariant).
unsafe impl<T: Send> Sync for SpscProducer<T> {}

impl<T> SpscProducer<T> {
    /// Push one item. Returns `false` if the ring is full (item dropped).
    #[inline]
    pub fn push(&self, item: T) -> bool {
        // SAFETY: single producer — exclusive access to the producer half per
        // the SPSC invariant the caller upholds.
        let producer = unsafe { &mut *self.producer };
        producer.try_push(item).is_ok()
    }
}

impl<T> Clone for SpscProducer<T> {
    fn clone(&self) -> Self {
        Self {
            producer: self.producer,
        }
    }
}
