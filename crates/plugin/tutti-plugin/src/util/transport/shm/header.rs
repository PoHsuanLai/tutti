//! The slab's synchronization header: what makes a read *meaningful*.
//!
//! # Why this exists
//!
//! The audio regions below this header carry no evidence of who wrote them: a
//! freshly-mapped region of zeros and one the peer just filled are byte-identical
//! to the reader. That is exactly how the out-of-process bypass shipped — with
//! input and output aliased, an unmatched read handed the host its own input back
//! at unity gain, and every test passed because a plausible signal came out.
//!
//! So the evidence goes *in the shared region*, beside the data it describes: a
//! per-slot sequence number the writer stamps after the last sample and the
//! reader checks before the first. This is the shape JACK and PipeWire ship, and
//! it is what finally justifies [`MmapCell`]'s `unsafe impl Sync` — the
//! justification can cite a Release/Acquire pair rather than a handshake in a
//! layer the type system cannot see.
//!
//! [`MmapCell`]: super::mmap::MmapCell
//!
//! # The ordering, and why it is not decorative
//!
//! Publish is `memcpy` every channel, *then* store `seq` with
//! [`Release`](Ordering::Release). Read is the mirror: load `seq` with
//! [`Acquire`](Ordering::Acquire), and only `memcpy` out if it matches.
//!
//! Without the pair a reader may legally observe the new sequence number
//! alongside the *old* samples — CPU and compiler are both free to reorder plain
//! stores around a `Relaxed` atomic, and on aarch64 (one of the two architectures
//! this engine targets) that is real hardware behaviour, not just a permitted
//! one.
//!
//! This property cannot be tested into existence: a stress test on
//! strongly-ordered x86-64 passes either way, and on aarch64 it fails only
//! probabilistically. **Code review is the net here, not the test suite.** Each
//! ordering appears in exactly one place below, with its reason for not being
//! `Relaxed`.
//!
//! # Sequence numbers, not a "newest" index
//!
//! Per-slot, because the reader wants a *specific* block — the one it submitted
//! a fixed number of blocks ago — not the newest. A single newest-index would let
//! a skipped block (server error, shm never set up) hand back N-2 as though it
//! were N-1: the same defect class as the original bug, one block quieter.
//!
//! Sequences start at **1**: a fresh slab is zeroed, so 0 must mean "nothing was
//! ever published here".

use crossbeam::utils::CachePadded;
use std::mem::{align_of, offset_of, size_of};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::error::BridgeError;

/// Identifies a tutti audio slab. Checked by the opening side before it trusts
/// a single byte of the mapping.
pub(super) const SLAB_MAGIC: u64 = u64::from_le_bytes(*b"TTI_SLAB");

/// Bumped when this header's *layout* changes. Distinct from the wire
/// `PROTOCOL_VERSION`: that one gates the control-socket message shapes, this
/// one gates the bytes at offset 0 of the mapping. Both sides are the same
/// binary in practice, so a mismatch means something is badly wrong — but the
/// check is nearly free and the failure it prevents (mapping a wrong-shaped
/// region and reading it as audio) is not.
pub(super) const HEADER_VERSION: u32 = 1;

/// Ring depth. Two slots is the minimum that lets one block be published while
/// another is read, which is all the pipelined protocol needs: the bridge pumps
/// serially, so at most one block is ever in flight.
///
/// Fixed rather than negotiated because the reasoning above is not a tunable —
/// raising it is only meaningful alongside a bridge that pipelines its own
/// socket traffic, and that change would have to revisit `MAX_BEHIND` in
/// `dispatch.rs` at the same time (they are one constraint; see its docs).
pub const RING_SLOTS: usize = 2;

/// Setup-time identification. Separated from the sequence arrays so it shares no
/// cache line with them: it is written once and then read never on the hot path,
/// whereas the arrays are stored to every block by both processes.
#[repr(C)]
struct Control {
    magic: AtomicU64,
    header_version: AtomicU32,
    slots: AtomicU32,
}

/// The fixed-size prologue at offset 0 of every slab mapping.
///
/// `repr(C)` is load-bearing, not stylistic. Two independently-compiled
/// mappings of the same bytes must compute identical field offsets, and Rust's
/// default layout is explicitly unspecified — a compiler upgrade is permitted to
/// reorder fields, which would silently point one process's `output_seq` at the
/// other's `input_seq`. The `const` assertions below pin the properties that
/// matter; `repr(C)` is what makes them meaningful across builds.
#[repr(C)]
pub(super) struct SlabHeader {
    control: CachePadded<Control>,
    /// Written by the host, read by the server.
    input_seq: CachePadded<[AtomicU64; RING_SLOTS]>,
    /// Written by the server, read by the host.
    output_seq: CachePadded<[AtomicU64; RING_SLOTS]>,
}

// Each writer's sequence array must sit on its own cache line.
//
// This is a *performance* property, not a correctness one — false sharing makes
// the atomics slower, never wrong; correctness rests entirely on the
// Release/Acquire pair. But the cost is real and lands in the worst possible
// place: the host stores `input_seq` and the server stores `output_seq` every
// block, ~750 times a second each, from different processes on different cores.
// Sharing a line would ping-pong it between caches on the audio thread's
// critical path, paying the full price of contention over data that is not
// actually shared.
//
// `CachePadded` rather than a hand-rolled `align(64)`: the line size is not 64
// everywhere. It is 64 on x86-64 — but its L2 prefetcher pulls line *pairs*, so
// the effective granularity is 128 — and 128 on aarch64. Both are targets here,
// so a hardcoded 64 would be mistuned on both. `crossbeam` already carries the
// per-architecture constant and we already depend on it.
//
// The assertions check *properties* (each region is line-aligned) rather than an
// absolute `size_of`, because `CachePadded`'s size is target-dependent by
// design. That is the better guard anyway: it states the invariant instead of a
// number that happens to satisfy it today. Target-dependent size is safe for
// shared memory here because both processes are the same binary on the same
// machine — the header version check catches it if that ever stops being true.
const _: () = assert!(align_of::<CachePadded<[AtomicU64; RING_SLOTS]>>() >= 64);
const _: () = assert!(
    offset_of!(SlabHeader, input_seq) % align_of::<CachePadded<[AtomicU64; RING_SLOTS]>>() == 0
);
const _: () = assert!(
    offset_of!(SlabHeader, output_seq) % align_of::<CachePadded<[AtomicU64; RING_SLOTS]>>() == 0
);
// The two arrays must not land on the same line even if the padding were
// misconfigured — this is the invariant the whole block above exists to secure.
const _: () = assert!(
    offset_of!(SlabHeader, output_seq) - offset_of!(SlabHeader, input_seq)
        >= align_of::<CachePadded<[AtomicU64; RING_SLOTS]>>()
);

/// Bytes reserved for the header at the start of every slab. The audio regions
/// begin here.
pub(super) const SLAB_HEADER_BYTES: usize = size_of::<SlabHeader>();

/// Which direction a sequence belongs to. The two arrays are structurally
/// identical, so an untyped index would make "host writes the server's array" a
/// silent bug rather than a compile error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Direction {
    /// Host → server. The host publishes, the server checks.
    Input,
    /// Server → host. The server publishes, the host checks.
    Output,
}

impl SlabHeader {
    /// Stamp a newly created slab. Called only by the creating side, before the
    /// peer has been told the slab exists.
    ///
    /// The magic is stored **last**, with `Release`, so it doubles as the
    /// publication point for the rest of the header: a peer that observes the
    /// magic with `Acquire` (see [`validate`](Self::validate)) is guaranteed to
    /// see the version and slot count that were written before it.
    pub(super) fn initialize(&self) {
        self.control
            .header_version
            .store(HEADER_VERSION, Ordering::Relaxed);
        self.control
            .slots
            .store(RING_SLOTS as u32, Ordering::Relaxed);
        for seq in self.input_seq.iter().chain(self.output_seq.iter()) {
            seq.store(0, Ordering::Relaxed);
        }
        self.control.magic.store(SLAB_MAGIC, Ordering::Release);
    }

    /// Check that this mapping is a tutti slab of a recognized shape.
    ///
    /// `Acquire` on the magic pairs with the `Release` in
    /// [`initialize`](Self::initialize); the two `Relaxed` loads after it are
    /// correct precisely *because* of that pairing — the acquire already
    /// establishes happens-before with everything the creator wrote first, so
    /// re-synchronizing on each field would be redundant.
    pub(super) fn validate(&self) -> Result<(), BridgeError> {
        let magic = self.control.magic.load(Ordering::Acquire);
        if magic != SLAB_MAGIC {
            return Err(BridgeError::SharedMemoryError(format!(
                "not a tutti audio slab: magic {magic:#x}, expected {SLAB_MAGIC:#x}"
            )));
        }
        let version = self.control.header_version.load(Ordering::Relaxed);
        if version != HEADER_VERSION {
            return Err(BridgeError::SharedMemoryError(format!(
                "slab header version {version}, this build speaks {HEADER_VERSION}"
            )));
        }
        let slots = self.control.slots.load(Ordering::Relaxed);
        if slots as usize != RING_SLOTS {
            return Err(BridgeError::SharedMemoryError(format!(
                "slab has {slots} ring slots, this build expects {RING_SLOTS}"
            )));
        }
        Ok(())
    }

    fn array(&self, direction: Direction) -> &[AtomicU64; RING_SLOTS] {
        match direction {
            Direction::Input => &self.input_seq,
            Direction::Output => &self.output_seq,
        }
    }

    /// Announce that `slot` now holds block `seq`, in `direction`.
    ///
    /// **Call this exactly once, after the last channel's samples are in
    /// place.** Publishing per-channel would let a reader observe a slot marked
    /// valid while later channels are still being copied — the block would be
    /// half this block and half the previous one, which is worse than silence
    /// because it sounds almost right.
    ///
    /// `Release` is what makes the preceding sample writes visible to a reader
    /// that acquires this same location. It is the entire safety argument for
    /// the region below; see the module docs.
    #[inline]
    pub(super) fn publish(&self, direction: Direction, slot: usize, seq: u64) {
        self.array(direction)[slot].store(seq, Ordering::Release);
    }

    /// The block currently published in `slot`, or 0 if nothing ever was.
    ///
    /// `Acquire` pairs with [`publish`](Self::publish)'s `Release`: once this
    /// returns the expected sequence, every sample write that preceded that
    /// publish is visible to this thread. Reading the samples before this load,
    /// or loading `Relaxed`, would defeat the ordering entirely.
    #[inline]
    pub(super) fn sequence(&self, direction: Direction, slot: usize) -> u64 {
        self.array(direction)[slot].load(Ordering::Acquire)
    }
}

/// The ring slot block `seq` lives in.
///
/// A free function rather than a method because both processes compute it from
/// a sequence number alone, and it must give the same answer on both sides —
/// making it a method on the header would invite one side to derive it from
/// something else.
#[inline]
pub(super) fn slot_for(seq: u64) -> usize {
    (seq % RING_SLOTS as u64) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a zeroed header in ordinary memory. Real slabs live in an mmap, but
    /// the header's logic is address-agnostic and a `Box` exercises it without a
    /// filesystem.
    fn header() -> Box<SlabHeader> {
        // SAFETY: `SlabHeader` is all-atomic `repr(C)` with no padding
        // invariants and no niches, so the all-zero bit pattern is a valid
        // instance — that is precisely the state a freshly-created mmap is in.
        unsafe { Box::new(std::mem::zeroed()) }
    }

    /// The state a fresh mapping is in: zeros mean "nothing published", which is
    /// why sequences start at 1.
    #[test]
    fn a_zeroed_slab_has_published_nothing() {
        let h = header();
        for slot in 0..RING_SLOTS {
            assert_eq!(h.sequence(Direction::Input, slot), 0);
            assert_eq!(h.sequence(Direction::Output, slot), 0);
        }
        assert!(
            h.validate().is_err(),
            "zeros are not a valid slab — the magic must be stamped"
        );
    }

    #[test]
    fn initialize_then_validate_round_trips() {
        let h = header();
        h.initialize();
        assert!(h.validate().is_ok());
    }

    /// Corrupt magic is rejected. This is the first line of defence against
    /// mapping something that is not ours and reading it as audio.
    #[test]
    fn corrupt_magic_is_rejected() {
        let h = header();
        h.initialize();
        h.control.magic.store(0xDEAD_BEEF, Ordering::Release);
        let err = h.validate().unwrap_err().to_string();
        assert!(err.contains("not a tutti audio slab"), "{err}");
    }

    #[test]
    fn a_future_header_version_is_rejected() {
        let h = header();
        h.initialize();
        h.control
            .header_version
            .store(HEADER_VERSION + 1, Ordering::Relaxed);
        let err = h.validate().unwrap_err().to_string();
        assert!(err.contains("header version"), "{err}");
    }

    /// A peer built with a different ring depth must be refused rather than
    /// silently reading the wrong slot.
    #[test]
    fn a_mismatched_slot_count_is_rejected() {
        let h = header();
        h.initialize();
        h.control
            .slots
            .store(RING_SLOTS as u32 + 2, Ordering::Relaxed);
        let err = h.validate().unwrap_err().to_string();
        assert!(err.contains("ring slots"), "{err}");
    }

    /// The two directions are independent storage. If they aliased, the host's
    /// own input publish would satisfy its own output check — which is the
    /// bypass bug, reconstructed one level up.
    #[test]
    fn the_two_directions_do_not_alias() {
        let h = header();
        h.initialize();
        h.publish(Direction::Input, 0, 7);
        assert_eq!(h.sequence(Direction::Input, 0), 7);
        assert_eq!(
            h.sequence(Direction::Output, 0),
            0,
            "publishing an input must not mark an output as ready"
        );
    }

    /// Slots are independent, so a stale slot cannot masquerade as a fresh one.
    #[test]
    fn slots_are_independent() {
        let h = header();
        h.initialize();
        h.publish(Direction::Output, 0, 10);
        assert_eq!(h.sequence(Direction::Output, 1), 0);
    }

    /// `slot_for` must agree with the ring depth and be stable across the u64
    /// range — both processes derive the slot from the sequence alone.
    #[test]
    fn slot_for_wraps_with_the_ring() {
        for seq in 0u64..8 {
            assert_eq!(slot_for(seq), (seq as usize) % RING_SLOTS);
        }
        // Consecutive blocks always land in different slots at depth 2 — this is
        // what lets one be read while the next is written.
        for seq in 0u64..8 {
            assert_ne!(slot_for(seq), slot_for(seq + 1));
        }
    }

    /// The header must not grow into the audio regions' space unnoticed, and the
    /// regions must start on an 8-byte boundary so `f64` samples are aligned.
    #[test]
    fn header_size_is_sane_and_sample_aligned() {
        assert!(SLAB_HEADER_BYTES >= size_of::<u64>() * 4);
        assert_eq!(
            SLAB_HEADER_BYTES % align_of::<f64>(),
            0,
            "audio regions start right after the header and must stay aligned"
        );
    }
}
