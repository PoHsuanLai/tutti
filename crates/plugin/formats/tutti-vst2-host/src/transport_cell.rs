//! The transport snapshot the plugin reads back via `audioMasterGetTime`.
//!
//! # Why this is not an `ArcSwap`
//!
//! It was one. `update_transport` ran `time_info.store(Arc::new(Some(next)))`
//! once per block, on the audio thread, on the primary path — so every block
//! with a transport (the common case) did a heap allocation *and* a free of
//! the retired snapshot, both inside the audio callback. That is the exact
//! shape CLAUDE.md's "Publishing to the Audio Thread" section forbids:
//! *"Never `publish` from the audio thread — it stalls the callback and frees
//! inside it."* Here the audio thread was the publisher.
//!
//! `RtPublish` is the project's answer for control-thread→audio-thread
//! publishing, and it is the wrong tool here for two reasons: the direction is
//! reversed (the audio thread writes; the plugin — possibly on another thread —
//! reads), and it would keep the allocation, since `RtPublish::publish` also
//! takes ownership of a fresh value. What this site actually needs is a cell
//! that is *mutated in place* rather than replaced.
//!
//! # Why a seqlock
//!
//! [`vst::api::TimeInfo`] is ~100 bytes of plain `Copy` POD, and the reader
//! (`Host::get_time_info`) returns it **by value** — `vst-tutti`'s
//! `host_dispatch` copies the returned `Option<TimeInfo>` into a thread-local
//! `Cell` and hands the plugin a pointer to *that*. So no reader ever needs an
//! owning handle to the published value, which is what made the `Arc` pure
//! overhead. A seqlock gives wait-free reads and an allocation-free,
//! lock-free write of a value too large for a single atomic.
//!
//! # Re-entrancy, which is the whole design constraint
//!
//! The plugin calls `audioMasterGetTime` from *inside* `processReplacing`,
//! i.e. re-entrantly on the audio thread — the same thread that just wrote.
//! A textbook seqlock reader spins until the sequence number is even and
//! stable, which on a same-thread re-entrant read during a write would spin
//! forever. That deadlock is unreachable here by construction:
//! [`Vst2Instance::process_f32`] calls `update_transport` to completion
//! *before* it calls into the plugin, so by the time the plugin can issue a
//! re-entrant read the sequence is already even. [`write`] additionally
//! asserts the counter was even on entry in debug builds, which is what pins
//! that ordering — reorder the two calls and the debug assertion fires rather
//! than the release build hanging.
//!
//! A reader on a *different* thread (the GUI thread polling transport while
//! audio renders) is the genuinely concurrent case, and it is why [`read`]
//! retries a bounded number of times and then reports "no transport" instead
//! of spinning. A plugin briefly told the transport is unavailable degrades;
//! a host that spins on the GUI thread hangs the UI.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU32, Ordering};

/// How many times [`TransportCell::read`] retries a torn read before giving up.
///
/// The writer's critical section is a single ~100-byte struct copy with no
/// branches or syscalls, so a reader that loses the race twice in a row has
/// been descheduled mid-read rather than merely raced. Retrying further would
/// not help, and the fallback (`None` — "host has no transport info", a
/// response every VST2 plugin must already handle, since it is what a host
/// without a transport returns) is strictly better than an unbounded spin on
/// whatever thread the plugin chose to call from.
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

// SAFETY: `value` is only ever touched through the seqlock protocol. Writes
// happen under an odd sequence number with `Release` ordering on both edges;
// reads copy the value out between two `Acquire` sequence loads and discard the
// copy unless the sequence was even and unchanged, so a reader never returns
// bytes from an in-flight write. `TimeInfo` is `Copy` POD with no interior
// pointers, so a torn *discarded* copy cannot have observed a dangling
// reference — the worst case is meaningless numbers that are then thrown away.
//
// This type tolerates one writer only. `write` is `&mut self`-gated through
// `Vst2Instance`, which is `!Sync` in practice for writes (callers serialize:
// the subprocess server is single-threaded, the in-process backend holds a
// `parking_lot::Mutex`), so the single-writer requirement is upheld by the
// same discipline that already governs `process_f32`.
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

    /// Publish a new snapshot. Allocation-free and lock-free: the value is
    /// overwritten in place, so nothing is allocated and nothing is freed.
    ///
    /// Single-writer only — see the `Sync` safety note on the type.
    pub(crate) fn write(&self, next: vst::api::TimeInfo) {
        let seq = self.seq.load(Ordering::Relaxed);
        // The re-entrancy argument in the module docs rests on writes never
        // overlapping, on this thread or any other. If this trips, a second
        // writer exists or `update_transport` has been moved inside the
        // plugin's `process` call, and the release build would be handing
        // plugins torn snapshots instead of failing here.
        debug_assert!(
            seq.is_multiple_of(2),
            "TransportCell had a write already in flight"
        );

        self.seq.store(seq.wrapping_add(1), Ordering::Relaxed);
        // Both fences are load-bearing: the first keeps the value write from
        // being hoisted above the odd-marking store, the second keeps it from
        // sinking below the even-marking store.
        std::sync::atomic::fence(Ordering::Release);
        // SAFETY: the sequence number is odd for the duration of this write, so
        // any concurrent reader discards whatever it copies. Single-writer
        // means no other thread is writing concurrently.
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
            // SAFETY: `TimeInfo` is `Copy` POD. This read may race with a
            // writer and produce torn bytes, which is exactly why the result is
            // discarded unless the sequence check below confirms no write
            // overlapped it. No interior pointers means torn bytes are merely
            // wrong numbers, never an invalid reference.
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

    /// The re-entrant shape the plugin actually produces: a read issued from
    /// inside `process`, i.e. after the write has completed on the same thread.
    /// This must return the fresh value and must not spin.
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

    /// A concurrent reader must never observe a half-written snapshot. Every
    /// field the writer sets moves together, so any torn read shows up as an
    /// inconsistent pair.
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
