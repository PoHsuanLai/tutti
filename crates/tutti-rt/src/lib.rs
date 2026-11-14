#![doc = include_str!("../README.md")]
//! Real-time-safe shared-memory primitives for the audio thread.
//!
//! ## What this is
//!
//! The structures the engine's audio callback touches, and the home of the
//! engine's lock-free and `unsafe` runtime structures. It names no tutti crate: its dependencies are
//! `arc-lend`, `arrayvec` and `bytemuck` (and `loom` under `--cfg loom`), so any crate may take it without
//! taking the unit vocabulary.
//!
//! - **`AudioThreadCell`** — interior mutability with a one-borrow-at-a-time
//!   contract checked in every build, reached through `&self`.
//! - **`RtVec`** — a capped collection through `&mut self`: refuses the write at the
//!   cap rather than growing.
//! - **`PosRing`** — frames a writer thread places ahead of the audio thread,
//!   indexed by position (a disk stream's ring).
//! - **`PackedAtomic`**, **`SeqWriter`/`SeqReader`**, **`RequestWord`**,
//!   **`edit_ring`**, **`DirtySet`** — a value in one atomic word, a sequence lock, a request
//!   bitset, a bounded single-producer single-consumer queue, and a sized set of
//!   dirty flags (an atomic bit per index) with no capacity to overflow.
//! - **`RtScope`** — a per-thread "inside a block" marker that a `Drop` impl checks.
//! - **`SplitMut`/`SplitRw`**, **`TaskGraph`**, **`JobGate`**, **`BlockWaits`** — the
//!   parallel executor's claims, task list, job lending and wait settings.
//! - **`with_no_denormals`** — runs a closure with subnormals flushed to zero and restores the mode after, unwinding included.
//! - **`level_cell()`** — a peak and an energy the audio thread folds each block
//!   into and one reader takes whole since its last take (a level meter's lane).
//! - **`Doorbell`** — wakes one parked thread from the audio thread (a futex wake only
//!   if a sleeper is parked), with no wake-up lost between the sleeper's check and its park.
//! - **`request_priority`** — asks the OS to raise the calling thread's priority and returns
//!   what it granted (never real time on Linux), or the refusal.
//! - **`ArcLend`/`Loan`/`Reader`** — the publish cell, re-exported from `arc-lend`:
//!   a control thread publishes non-scalar state, the audio thread only borrows it.
//!
//! **The root is the API.** Every item is re-exported at the root
//! (`tutti_rt::RtVec`); the modules are private.
//!
//! Each lock-free structure has a loom model in `tests/*_loom.rs` that runs the
//! shipped code:
//!
//! ```text
//! cargo test -p tutti-rt --release --test pos_ring_loom   # with the loom cfg flag set
//! ```
//!
//! ## Safety
//!
//! Outside the `-ffi` crates (the plugin formats, the OS MIDI APIs, the plugin
//! process edge), this crate holds all of the engine's `unsafe`, and the external `arc-lend`
//! crate holds the publish cell's;
//! every other library carries `#![forbid(unsafe_code)]`. Each site exists because the safe construct would
//! have cost something on the audio thread:
//!
//! - `AudioThreadCell` (`cell.rs`): an `UnsafeCell` with `unsafe impl Sync`, made
//!   true by one atomic word (a writer bit and a reader count) checked on every
//!   borrow in every build. A conflicting borrow panics, or its `try_` form
//!   returns `None`, instead of aliasing.
//! - `edit_ring` (`edits.rs`): single-producer single-consumer slots of
//!   `UnsafeCell<u64>`, each touched only on its side of the `Release`/`Acquire`
//!   indices.
//! - `SplitMut`, `SplitRw` and `ClaimTable` (`split.rs`): one slice borrowed by
//!   several threads, each element or chunk claimed in an atomic table before a
//!   reference to it is made, so a second claim is refused rather than aliased.
//!   They are why `tutti-graph`'s executor can hand every node its buffers out of
//!   one arena under `#![forbid(unsafe_code)]`.
//! - `JobGate` (`job_gate.rs`): erases a borrowed job's lifetime for a pool's
//!   helpers; `run` neither returns nor unwinds until no helper is inside the job
//!   (a store-load pair with `SeqCst` fences).
//! - `handoff` (`handoff.rs`): one `MaybeUninit` slot moved between the two ends
//!   under a state word.
//! - `with_no_denormals` (`denormals.rs`): saves, sets and restores the FPU
//!   control register with `asm!`, which has no safe spelling; its `# Soundness`
//!   docs say what it does and does not guarantee.
//! - `monotonic_ns` (`clock.rs`): the OS clock calls.
//!
//! `arc-lend`'s `ArcLend` (an external crate) holds the rest: an `AtomicPtr` to an `Arc::into_raw`
//! pointer that a `Loan` dereferences under a hazard-slot protocol, so the audio
//! thread can never free a value (the reason it exists instead of `ArcSwap`).
//!
//! What checks it: a loom model per lock-free structure, run against the shipped
//! code (`tests/*_loom.rs` here, and arc-lend's own loom model for `ArcLend`), and miri
//! over this crate's unit tests (arc-lend's CI runs its own), in CI's `loom` and
//! `miri` jobs (`cargo xtask loom`, `cargo xtask miri`). Miri executes no FFI and no x86 intrinsics,
//! so the denormal register tests are skipped under it, and it perturbs libm
//! results, so a green run says nothing about numerics. The plugin formats'
//! `unsafe` is answered structurally instead: a plugin that corrupts memory
//! takes down the out-of-process `plugin-server`, not the host.
#![deny(missing_docs)]
#![deny(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]

//! Real-time-safe shared-memory primitives for the audio thread.
//!
//! Interior mutability, fixed-capacity buffers, and the denormals flush: each
//! is allocation-free on the audio path (`tests/rt_no_alloc.rs` gates it) and
//! carries no engine dependency. The crate also holds the one thread-priority
//! call, [`request_priority`], which says what the OS granted.
//!
//! # How the buffers relate
//!
//! There is one capped collection, [`RtVec`]: it answers "what happens at the
//! cap?" — refuse the write, latch an overflow flag, never grow — with
//! `&mut self` in and a borrowed `&[T]` out. An owner that holds the buffer
//! behind an `Arc` (a CPAL callback's shared state, a COM object) puts the
//! `RtVec` in an
//! [`AudioThreadCell`] and reads it under that cell's borrow guard; it never
//! lends a borrow into the storage out of a `&self` method, which would need
//! an `UnsafeCell` and a single-thread contract the compiler cannot check.
//!
//! A scratch run whose length is chosen per block is a plain `Box<[T]>`
//! sized off the audio thread and sliced at the use site, which states there
//! what happens when a block asks for more than it holds.
//!
//! - [`AudioThreadCell`] — interior mutability with a "one borrow at a time"
//!   contract, checked on every borrow in every build, reached through
//!   `&self` (e.g. behind an `Arc` or a COM object).
//! - [`RtVec`] — collect-then-lend through `&mut self`: a capped collection
//!   that owns its storage and exposes it as `&[T]`. What a per-block pool
//!   wants when its owner already has `&mut self`.
//! - [`PosRing`] — frames a writer thread places ahead of the audio thread,
//!   indexed by position rather than consumed in order (a disk stream's ring):
//!   one [`PosWriter`] and one [`PosReader`], whose [`PosClaim`] reads any
//!   position the window it took holds.
//! - [`PackedAtomic`] — a value of up to eight bytes (a range's two `f32`
//!   bounds) in one atomic word, so a reader never pairs one write's field
//!   with another's.
//! - [`SeqWriter`] / [`SeqReader`] — a `Pod` value one writer (the audio
//!   thread, each block) overwrites in place and any thread copies whole: a
//!   sequence lock, for a value whose writer may not allocate or free. The
//!   writer is not `Clone`, so there is only one; a read that keeps losing
//!   to writes says so ([`Contended`]) rather than spinning.
//! - [`level_cell()`] — a peak and an energy the audio thread folds each block
//!   into and one reader takes whole, since its last take: two slots and a
//!   flip, so a block lands in one take with its peak and frames together.
//! - [`RequestWord`] — up to 32 kinds of request a callee raises from any
//!   thread (one `fetch_or`) and a control thread takes whole.
//! - [`WakeFlag`] — a flag any thread raises (one store) and the audio thread
//!   takes (raised since the last take), so a parked node can be woken from
//!   outside the block.
//! - [`edit_ring()`] — a bounded single-producer single-consumer queue of small
//!   `Copy` values, for edits the control thread hands the audio thread.
//! - [`drain_clock()`] — counts the drains the audio side begins, so the control
//!   side can tell which of its writes a block has surely taken.
//! - [`FaultLatch`] — a flag a producer raises when it stops for good, read
//!   *before* the ring so a failure never hides the frames pushed ahead of it.
//! - [`handoff()`] — one value that moves between a control thread and the
//!   audio thread at a block boundary ([`Hand`] lends and recalls, [`Rt`]
//!   trades between blocks): a plugin's Processor, lent to the graph and
//!   recalled for a rate change.
//! - [`Doorbell`] — wakes one parked thread from another (the audio thread
//!   rings; a futex wake only if a sleeper is parked), with no wake-up lost
//!   between its check and its park.
//! - [`RtScope`] — a per-thread "inside a block" marker, which a `Drop`
//!   impl checks ([`RtScope::check_outside`]) so a value that must be
//!   freed on the control thread panics, in debug builds, when it is not.
//! - [`SplitMut`] / [`SplitRw`] — one slice borrowed by several threads at
//!   once, each element (or chunk) claimed before use, so a borrow that would
//!   alias is refused rather than made: a parallel executor's buffers.
//! - [`TaskGraph`] — a DAG of tasks run once per block by any number of
//!   threads: self-resetting activation counters and a ready list.
//! - [`JobGate`] — lends a borrowed job to a pool's helper threads for one
//!   call and returns only when every helper has let go of it.
//! - [`BlockWaits`] — how long the audio thread and a helper each wait
//!   inside a parallel block before giving way (yielding its core, leaving
//!   the block): a pool's setting.
//! - [`with_no_denormals`] — runs a closure with subnormals flushed to zero, then
//!   restores the FPU control register, on unwind too.

mod cell;
mod clock;
mod denormals;
mod dirty_set;
mod doorbell;
mod drain_clock;
mod edits;
mod fault_latch;
mod handoff;
mod job_gate;
mod level_cell;
mod packed_atomic;
mod pos_ring;
mod priority;
mod reclaim;
mod request_word;
mod scope;
mod seq_cell;
mod spin;
mod split;
mod task_graph;
mod vec;
mod wake_flag;

pub use cell::{AudioThreadCell, BorrowGuard, BorrowRef};
pub use clock::monotonic_ns;
pub use denormals::with_no_denormals;
pub use dirty_set::{DirtyIndex, DirtySet, Taken};
pub use doorbell::Doorbell;
pub use drain_clock::{DrainBeat, DrainMark, DrainWatch, drain_clock};
pub use edits::{EditReader, EditWriter, RingFull, edit_ring};
pub use fault_latch::{Drained, FaultLatch};
pub use handoff::{Hand, Rt, handoff};
pub use job_gate::JobGate;
pub use level_cell::{LevelReader, LevelSpan, LevelWriter, level_cell};
pub use packed_atomic::PackedAtomic;
pub use pos_ring::{
    MAX_POS_RING_FRAMES, MAX_POS_RING_POSITION, PosClaim, PosFrame, PosReader, PosRing, PosWriter,
    RingWindow,
};
pub use priority::{PriorityOutcome, PriorityRequest, Scheduling, request_priority};
pub use reclaim::Reclaim;
pub use request_word::{REQUEST_BITS, RequestBit, RequestBitOutOfRange, RequestBits, RequestWord};
pub use scope::{RtScope, RtScopeGuard};
pub use seq_cell::{Contended, SeqReader, SeqWriter};
pub use spin::BlockWaits;
pub use split::{ClaimConflict, ClaimTable, Claimed, ClaimedRun, Held, RwView, SplitMut, SplitRw};
pub use task_graph::{Participant, TaskGraph};
pub use vec::RtVec;
pub use wake_flag::WakeFlag;

// The cell non-scalar state reaches the audio thread through: a control
// thread publishes, the audio thread only borrows (`Loan`), so it never frees.
// The external `arc-lend` crate, which names nothing of tutti's; re-exported here
// so the engine spells it `tutti_rt::ArcLend`.
pub use arc_lend::{ArcLend, Loan, Reader};
