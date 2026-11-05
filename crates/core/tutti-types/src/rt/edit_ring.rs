//! [`edit_ring`]: a bounded single-producer single-consumer queue of small
//! `Copy` values, for edits the control thread hands the audio thread.
//!
//! The producer is the control thread (a parameter edit on an active plugin);
//! the consumer is the audio thread, which drains the ring at the head of each
//! block. Neither side allocates, locks or waits after construction. A full
//! ring refuses the value and counts it; the producer keeps the latest value
//! and retries on its next tick.
//!
//! # The storage
//!
//! Each slot is a run of `u64` words, written and read `Relaxed`. Handing a
//! slot over is the index that follows it, so there is no data race to make
//! undefined and the module has no `unsafe`; `T` is [`NoUninit`] and
//! [`AnyBitPattern`], so the conversion both ways is `bytemuck`'s.
//!
//! # The protocol
//!
//! `head` counts values pushed and `tail` values popped, both only increasing.
//! A slot is `count % capacity`.
//!
//! - **Push:** load `tail` with `Acquire`; if `head - tail == capacity` the
//!   ring is full. Otherwise write the slot's words, then store `head + 1`
//!   with `Release`.
//! - **Pop:** load `head` with `Acquire`; if equal to `tail` the ring is
//!   empty. Otherwise read the slot's words, then store `tail + 1` with
//!   `Release`.
//!
//! The `Release` store of `head` publishes the slot's words to a consumer that
//! `Acquire`-loads that `head`; the `Release` store of `tail` tells the
//! producer the slot may be written again, after the consumer finished reading
//! it. Each side reads the other's index only to bound itself, so a stale read
//! can only make the ring look fuller or emptier than it is, never overrun
//! it: there is no store-then-load pair that needs a `SeqCst` fence.
//!
//! The model in `tests/edit_ring_loom.rs` runs this code.

use std::sync::Arc;

use bytemuck::{AnyBitPattern, NoUninit};
use sync::{AtomicU32, AtomicU64, Ordering};

mod sync {
    #[cfg(loom)]
    pub(super) use loom::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    #[cfg(not(loom))]
    pub(super) use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
}

const WORD: usize = std::mem::size_of::<u64>();

#[derive(Debug)]
struct Shared {
    head: AtomicU64,
    tail: AtomicU64,
    refused: AtomicU32,
    words_per_slot: usize,
    capacity: usize,
    words: Box<[AtomicU64]>,
}

/// A value that did not fit: the ring was full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingFull;

/// The producing end.
#[derive(Debug)]
pub struct EditWriter<T> {
    shared: Arc<Shared>,
    head: u64,
    _value: std::marker::PhantomData<fn(T)>,
}

/// The consuming end.
#[derive(Debug)]
pub struct EditReader<T> {
    shared: Arc<Shared>,
    tail: u64,
    _value: std::marker::PhantomData<fn() -> T>,
}

/// A ring holding up to `capacity` values, as its two ends.
///
/// # Panics
///
/// If `capacity` is zero.
///
/// ```
/// use tutti_types::edit_ring;
///
/// let (mut writer, mut reader) = edit_ring::<[u32; 2]>(2);
/// writer.push([1, 2]).unwrap();
/// writer.push([3, 4]).unwrap();
/// assert!(writer.push([5, 6]).is_err(), "full");
/// assert_eq!(writer.refused(), 1);
/// assert_eq!(reader.pop(), Some([1, 2]));
/// assert_eq!(reader.pop(), Some([3, 4]));
/// assert_eq!(reader.pop(), None);
/// ```
#[must_use]
pub fn edit_ring<T: NoUninit + AnyBitPattern>(capacity: usize) -> (EditWriter<T>, EditReader<T>) {
    assert!(capacity > 0, "an edit ring holds at least one value");
    let words_per_slot = std::mem::size_of::<T>().div_ceil(WORD);
    let words = (0..capacity * words_per_slot)
        .map(|_| AtomicU64::new(0))
        .collect();
    let shared = Arc::new(Shared {
        head: AtomicU64::new(0),
        tail: AtomicU64::new(0),
        refused: AtomicU32::new(0),
        words_per_slot,
        capacity,
        words,
    });
    (
        EditWriter {
            shared: shared.clone(),
            head: 0,
            _value: std::marker::PhantomData,
        },
        EditReader {
            shared,
            tail: 0,
            _value: std::marker::PhantomData,
        },
    )
}

impl Shared {
    fn slot(&self, count: u64) -> &[AtomicU64] {
        let at = (count % self.capacity as u64) as usize * self.words_per_slot;
        &self.words[at..at + self.words_per_slot]
    }
}

impl<T: NoUninit + AnyBitPattern> EditWriter<T> {
    /// Queues `value`, or refuses it and counts the refusal when the ring is
    /// full. Wait-free; never allocates.
    ///
    /// # Errors
    ///
    /// [`RingFull`] when `capacity` values are queued.
    pub fn push(&mut self, value: T) -> Result<(), RingFull> {
        let tail = self.shared.tail.load(Ordering::Acquire);
        if self.head - tail >= self.shared.capacity as u64 {
            self.shared.refused.fetch_add(1, Ordering::Relaxed);
            return Err(RingFull);
        }
        let bytes = bytemuck::bytes_of(&value);
        for (word, chunk) in self.shared.slot(self.head).iter().zip(bytes.chunks(WORD)) {
            let mut padded = [0; WORD];
            padded[..chunk.len()].copy_from_slice(chunk);
            word.store(u64::from_ne_bytes(padded), Ordering::Relaxed);
        }
        self.head += 1;
        self.shared.head.store(self.head, Ordering::Release);
        Ok(())
    }

    /// Values refused for want of room since the ring was made.
    #[must_use]
    pub fn refused(&self) -> u32 {
        self.shared.refused.load(Ordering::Relaxed)
    }
}

impl<T: NoUninit + AnyBitPattern> EditReader<T> {
    /// The oldest queued value, or `None` when the ring is empty. Wait-free;
    /// never allocates.
    pub fn pop(&mut self) -> Option<T> {
        let head = self.shared.head.load(Ordering::Acquire);
        if head == self.tail {
            return None;
        }
        let mut value = T::zeroed();
        let bytes = bytemuck::bytes_of_mut(&mut value);
        for (word, chunk) in self
            .shared
            .slot(self.tail)
            .iter()
            .zip(bytes.chunks_mut(WORD))
        {
            let loaded = word.load(Ordering::Relaxed).to_ne_bytes();
            chunk.copy_from_slice(&loaded[..chunk.len()]);
        }
        self.tail += 1;
        self.shared.tail.store(self.tail, Ordering::Release);
        Some(value)
    }

    /// Values the producer had refused for want of room.
    #[must_use]
    pub fn refused(&self) -> u32 {
        self.shared.refused.load(Ordering::Relaxed)
    }
}
