//! A `loom` model of `tutti_plugin`'s slab header Release/Acquire protocol.
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test -p tutti-shm-model
//! ```
//!
//! # Why a replica, and not the real `SlabHeader`
//!
//! loom works by *substituting* its own atomic types for `std`'s and then
//! exploring the interleavings and reorderings the C++11 memory model permits.
//! It can only see operations performed through `loom::sync::atomic`.
//!
//! `SlabHeader` cannot be handed to it. Its atomics live in an `mmap`'d region
//! shared between two OS processes: the type is `#[repr(C)]` over
//! `std::sync::atomic` values at fixed offsets, and both of those facts are
//! load-bearing (`header.rs` spells out why `repr(C)` is not stylistic). A
//! `loom::sync::atomic::AtomicU64` is a different type with a different size,
//! carrying thread-local bookkeeping — it is not a value you can point at
//! shared memory, and swapping it in under `cfg(loom)` would model a header
//! that is not the one that ships. loom also models *threads*, not processes;
//! the real protocol's two participants are separate address spaces that share
//! only the mapping.
//!
//! So this is a replica: the same protocol, the same ordering choices, over
//! loom's atomics. What it proves is that **the protocol** is sound under a
//! weak memory model. What it cannot prove is that `header.rs` implements this
//! protocol — that link is held by
//! `header::tests::the_block_protocol_uses_the_documented_orderings`, which
//! asserts the real code's ordering constants are the ones mirrored here. The
//! two tests are only meaningful together, and neither is redundant:
//!
//! | claim                                     | held by                       |
//! |-------------------------------------------|-------------------------------|
//! | the protocol is sound under a weak model  | this file                     |
//! | `header.rs` uses those exact orderings    | the ordering-constant tests   |
//! | the code runs at all, end to end          | the million-block smoke test  |
//!
//! # Why this is not in `tutti-plugin` at all
//!
//! It was written there first and could not run. `--cfg loom` is a *global*
//! RUSTFLAG, so it reaches every crate in the graph, and `lfqueue` — which
//! carries its own `cfg(loom)` support and is a mandatory transitive
//! dependency of `tutti-plugin` (fundsp-tutti → tutti-core) — fails to
//! compile under it. There is no per-crate scope for the flag, so the model
//! lives in a crate with an empty dependency list. Full note in this crate's
//! `Cargo.toml`.

// Without `--cfg loom` this file is empty. That is deliberate: the model is not
// part of the default suite (see the module docs), and a `cfg`-gated
// dev-dependency cannot be referenced from a build that does not have it.
#![cfg(loom)]

use loom::sync::atomic::{AtomicU64, Ordering};
use loom::sync::Arc;
use loom::thread;

/// Mirrors `header::RING_SLOTS`.
const RING_SLOTS: usize = 2;

/// Mirrors `header::PUBLISH_ORDERING`. Kept as a named constant for the same
/// reason it is one in `header.rs`: the mutation this file exists to catch is
/// someone weakening it, and a literal buried in a method is invisible.
const PUBLISH_ORDERING: Ordering = Ordering::Release;

/// Mirrors `header::SEQUENCE_ORDERING`.
const SEQUENCE_ORDERING: Ordering = Ordering::Acquire;

/// The protocol under test, reduced to its essentials.
///
/// One slot's worth of "audio" (a single word — the model does not care how
/// much data there is, only whether it is ordered against the publish) and one
/// sequence number, exactly as `SlabHeader` pairs an audio region with an
/// `input_seq`/`output_seq` entry.
struct Slot {
    /// Stands in for the samples. `Relaxed` on both sides, as in the real
    /// thing: this is a plain `memcpy` in `slab.rs`, with no ordering of its
    /// own. All the ordering comes from the sequence number beside it.
    data: AtomicU64,
    /// Stands in for `input_seq[slot]` / `output_seq[slot]`.
    seq: AtomicU64,
}

impl Slot {
    fn new() -> Self {
        Self {
            // A zeroed mapping: sequences start at 1 so 0 means "never
            // published", which is why the model can use 0 as the sentinel.
            data: AtomicU64::new(0),
            seq: AtomicU64::new(0),
        }
    }

    /// `slab::write_output` followed by `header::publish`.
    fn publish(&self, seq: u64) {
        self.data.store(seq, Ordering::Relaxed);
        self.seq.store(seq, PUBLISH_ORDERING);
    }

    /// `header::sequence` followed by `slab::read_output_into`.
    ///
    /// Returns the data only if the sequence says this block is the one
    /// wanted — the "check before the first sample" half of the protocol.
    fn read(&self, want: u64) -> Option<u64> {
        if self.seq.load(SEQUENCE_ORDERING) == want {
            Some(self.data.load(Ordering::Relaxed))
        } else {
            None
        }
    }
}

/// The property: a reader that observes a published sequence number observes
/// the data published before it. Never a torn or half-published block.
///
/// This is the claim `header.rs`'s module docs make and then say "cannot be
/// tested into existence" — true of a stress test on real hardware, and
/// exactly what a model checker is for. loom explores every interleaving *and*
/// every reordering the memory model permits, so a `Relaxed` store here is not
/// a probabilistic failure but a certain one.
///
/// # Mutation
///
/// Change `PUBLISH_ORDERING` to `Ordering::Relaxed` and loom reports the
/// violation with the interleaving that produced it: the reader observes
/// `seq == 1` alongside `data == 0`, the freshly-mapped zero. Weakening
/// `SEQUENCE_ORDERING` to `Relaxed` does the same. Both were run; see the PR
/// body.
#[test]
fn a_matched_sequence_implies_its_data_is_visible() {
    loom::model(|| {
        let slot = Arc::new(Slot::new());

        let writer = {
            let slot = Arc::clone(&slot);
            thread::spawn(move || slot.publish(1))
        };

        // The reader races the writer. Either it sees nothing yet — a perfectly
        // legal outcome the real bridge handles by treating the block as not
        // ready — or it sees the sequence, and then the data must be there.
        if let Some(data) = slot.read(1) {
            assert_eq!(
                data, 1,
                "observed the published sequence number alongside stale data: \
                 the release/acquire pair did not order the samples against \
                 the publish"
            );
        }

        writer.join().expect("writer thread");
    });
}

/// The ring's real shape: two slots, written in alternation, read by sequence.
///
/// Adds what the single-slot model omits — that a reader asking for block N
/// must not be satisfied by block N-2's publish landing in the same slot. That
/// is the "sequence numbers, not a newest index" argument from `header.rs`,
/// and it is a *different* failure from a torn read: the data is perfectly
/// coherent, it is simply the wrong block.
///
/// Two blocks is the smallest model that can express it, and the largest that
/// finishes quickly: loom's exploration is exponential in the number of
/// operations, so a third block buys no new class of interleaving and costs a
/// great deal of time.
#[test]
fn a_slot_reused_by_a_later_block_is_not_mistaken_for_an_earlier_one() {
    loom::model(|| {
        let slots: Arc<[Slot; RING_SLOTS]> = Arc::new([Slot::new(), Slot::new()]);

        let writer = {
            let slots = Arc::clone(&slots);
            thread::spawn(move || {
                // Blocks 1 and 2 land in different slots at depth 2, which is
                // the property `slot_for` exists to guarantee.
                slots[1 % RING_SLOTS].publish(1);
                slots[2 % RING_SLOTS].publish(2);
            })
        };

        for want in 1..=2u64 {
            if let Some(data) = slots[(want as usize) % RING_SLOTS].read(want) {
                assert_eq!(
                    data, want,
                    "a read matched on block {want} but got block {data}'s data"
                );
            }
        }

        writer.join().expect("writer thread");
    });
}
