//! A seqlock holding the transport snapshot the plugin reads back via
//! `audioMasterGetTime`. `TimeInfo` is ~100 bytes of `Copy` POD and readers
//! take it by value, so the cell is overwritten in place: wait-free reads,
//! allocation-free writes.
//!
//! Not an `ArcSwap`: the audio thread is the *writer* here (once per block),
//! and `store` would allocate the new snapshot and free the retired one inside
//! the callback — what CLAUDE.md's "Publishing to the Audio Thread" forbids.
//! `RtPublish` is control-thread→audio-thread and also takes ownership of a
//! fresh value, so it does not fit either.
//!
//! Re-entrancy is the design constraint: the plugin calls `audioMasterGetTime`
//! from inside `processReplacing`, on the same thread that just wrote, and a
//! seqlock reader cannot make progress against a write in flight on its own
//! thread. [`Vst2Instance::process_f32`] runs `update_transport` to completion
//! before entering the plugin, so the sequence is already even by then;
//! [`TransportCell::write`]'s debug assert pins that ordering. A reader on
//! another thread is the genuinely concurrent case, which is why [`read`] gives
//! up after a bounded number of retries rather than spinning.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU32, Ordering};

/// How many times [`TransportCell::read`] retries a torn read before giving up.
///
/// The writer's critical section is one ~100-byte copy, so a reader losing the
/// race repeatedly was descheduled rather than raced, and more retries will not
/// help. The fallback (`None` = "host has no transport", which every VST2
/// plugin must already handle) beats an unbounded spin on the plugin's thread.
const READ_RETRY_LIMIT: usize = 8;

/// A lock-free, allocation-free cell holding the latest transport snapshot.
///
/// Odd sequence number = a write is in flight; even = stable. Readers snapshot
/// the sequence, copy the value, and re-check — a changed or odd sequence means
/// the copy may be torn and is discarded.
pub(crate) struct TransportCell {
    /// Even when stable, odd while [`write`](Self::write) is mid-copy.
    seq: AtomicU32,
    /// `None` until the first transport snapshot is published. Written only
    /// under an odd `seq`; read only via a `seq`-validated copy.
    value: UnsafeCell<Option<vst::api::TimeInfo>>,
}

// SAFETY: `value` is only touched through the seqlock protocol — written under
// an odd sequence with `Release` on both edges, read between two `Acquire`
// sequence loads and discarded unless the sequence was even and unchanged.
// `TimeInfo` is `Copy` POD with no interior pointers, so a torn discarded copy
// is meaningless numbers, never an invalid reference.
//
// Single-writer only. Writes go through `Vst2Instance`, whose callers already
// serialize `process_f32` (single-threaded subprocess server; `parking_lot::
// Mutex` in the in-process backend).
unsafe impl Sync for TransportCell {}
unsafe impl Send for TransportCell {}

impl TransportCell {
    /// A cell holding no transport information yet.
    pub(crate) fn new() -> Self {
        Self {
            seq: AtomicU32::new(0),
            value: UnsafeCell::new(None),
        }
    }

    /// Publish a new snapshot, overwriting in place — nothing is allocated or
    /// freed. Single-writer only; see the `Sync` safety note on the type.
    pub(crate) fn write(&self, next: vst::api::TimeInfo) {
        let seq = self.seq.load(Ordering::Relaxed);
        // Pins the module docs' ordering: writes must never overlap. A trip
        // means a second writer exists, or `update_transport` moved inside the
        // plugin's `process` call.
        debug_assert!(
            seq.is_multiple_of(2),
            "TransportCell had a write already in flight"
        );

        self.seq.store(seq.wrapping_add(1), Ordering::Relaxed);
        // Both fences are load-bearing: the first keeps the value write from
        // being hoisted above the odd-marking store, the second keeps it from
        // sinking below the even-marking store.
        std::sync::atomic::fence(Ordering::Release);
        // SAFETY: the sequence is odd for the duration of this write, so any
        // concurrent reader discards what it copies; single-writer means no
        // other thread writes concurrently.
        unsafe {
            *self.value.get() = Some(next);
        }
        self.seq.store(seq.wrapping_add(2), Ordering::Release);
    }

    /// Read the current snapshot, or `None` if none has been published yet or
    /// the value could not be read cleanly within [`READ_RETRY_LIMIT`] tries.
    ///
    /// Wait-free: bounded retries, never blocks. Safe to call re-entrantly from
    /// the audio thread inside the plugin's `process`.
    pub(crate) fn read(&self) -> Option<vst::api::TimeInfo> {
        for _ in 0..READ_RETRY_LIMIT {
            let before = self.seq.load(Ordering::Acquire);
            if !before.is_multiple_of(2) {
                // A write is in flight; the value is not stable to copy.
                std::hint::spin_loop();
                continue;
            }
            // SAFETY: `TimeInfo` is `Copy` POD with no interior pointers, so a
            // race with the writer yields wrong numbers, never an invalid
            // reference — and the sequence re-check below discards those.
            let candidate = unsafe { *self.value.get() };
            std::sync::atomic::fence(Ordering::Acquire);
            if self.seq.load(Ordering::Acquire) == before {
                return candidate;
            }
            std::hint::spin_loop();
        }
        // Bounded-retry fallback — see `READ_RETRY_LIMIT`.
        None
    }
}

impl Default for TransportCell {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info_with(sample_pos: f64) -> vst::api::TimeInfo {
        vst::api::TimeInfo {
            sample_pos,
            ..Default::default()
        }
    }

    #[test]
    fn reads_none_before_any_write() {
        assert!(TransportCell::new().read().is_none());
    }

    #[test]
    fn read_returns_last_written_snapshot() {
        let cell = TransportCell::new();
        cell.write(info_with(128.0));
        assert_eq!(cell.read().unwrap().sample_pos, 128.0);
        cell.write(info_with(256.0));
        assert_eq!(cell.read().unwrap().sample_pos, 256.0);
    }

    /// The re-entrant shape: a read from inside `process`, after the write
    /// completed on the same thread. Must return the fresh value, not spin.
    #[test]
    fn reentrant_same_thread_read_sees_completed_write() {
        let cell = TransportCell::new();
        cell.write(info_with(64.0));
        // Nested reads, mimicking a plugin querying more than once per block.
        let outer = cell.read().unwrap();
        let inner = cell.read().unwrap();
        assert_eq!(outer.sample_pos, 64.0);
        assert_eq!(inner.sample_pos, 64.0);
    }

    /// A concurrent reader must never observe a half-written snapshot. The
    /// writer keeps every field it sets in lockstep, so a torn read shows up as
    /// a disagreeing pair.
    #[test]
    fn concurrent_reader_never_observes_a_torn_snapshot() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        let cell = Arc::new(TransportCell::new());
        let stop = Arc::new(AtomicBool::new(false));

        let reader = {
            let cell = Arc::clone(&cell);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut torn = 0usize;
                while !stop.load(Ordering::Relaxed) {
                    if let Some(info) = cell.read() {
                        // The writer keeps these three in lockstep, so any
                        // disagreement is a torn read.
                        if info.ppq_pos != info.sample_pos || info.tempo != info.sample_pos {
                            torn += 1;
                        }
                    }
                }
                torn
            })
        };

        for i in 0..200_000u32 {
            let v = f64::from(i);
            cell.write(vst::api::TimeInfo {
                sample_pos: v,
                ppq_pos: v,
                tempo: v,
                ..Default::default()
            });
        }
        stop.store(true, Ordering::Relaxed);

        assert_eq!(reader.join().unwrap(), 0, "reader observed a torn snapshot");
    }
}
