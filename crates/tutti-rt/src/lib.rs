#![doc = include_str!("../README.md")]
#![deny(missing_docs)]

//! Real-time-safe shared-memory primitives for the audio thread.
//!
//! Interior mutability, fixed-capacity buffers, and the denormals flush: each
//! is allocation-free on the audio path (`tests/rt_no_alloc.rs` gates it) and
//! carries no engine dependency.
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
//! - [`edit_ring()`] — a bounded single-producer single-consumer queue of small
//!   `Copy` values, for edits the control thread hands the audio thread.
//! - [`drain_clock()`] — counts the drains the audio side begins, so the control
//!   side can tell which of its writes a block has surely taken.
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
mod denormals;
mod doorbell;
mod drain_clock;
mod edits;
mod job_gate;
mod level_cell;
mod packed_atomic;
mod pos_ring;
mod priority;
mod request_word;
mod scope;
mod seq_cell;
mod spin;
mod split;
mod task_graph;
mod vec;

pub use cell::{AudioThreadCell, BorrowGuard, BorrowRef};
pub use denormals::with_no_denormals;
pub use doorbell::Doorbell;
pub use drain_clock::{DrainBeat, DrainMark, DrainWatch, drain_clock};
pub use edits::{EditReader, EditWriter, RingFull, edit_ring};
pub use job_gate::JobGate;
pub use level_cell::{LevelReader, LevelSpan, LevelWriter, level_cell};
pub use packed_atomic::PackedAtomic;
pub use pos_ring::{
    MAX_POS_RING_FRAMES, MAX_POS_RING_POSITION, PosClaim, PosFrame, PosReader, PosRing, PosWriter,
    RingWindow,
};
pub use priority::{PriorityOutcome, PriorityRequest, Scheduling, request_priority};
pub use request_word::{REQUEST_BITS, RequestBit, RequestBitOutOfRange, RequestBits, RequestWord};
pub use scope::{RtScope, RtScopeGuard};
pub use seq_cell::{Contended, SeqReader, SeqWriter};
pub use spin::BlockWaits;
pub use split::{ClaimConflict, ClaimTable, Claimed, ClaimedRun, Held, RwView, SplitMut, SplitRw};
pub use task_graph::{Participant, TaskGraph};
pub use vec::RtVec;

// The cell non-scalar state reaches the audio thread through: a control
// thread publishes, the audio thread only borrows (`Loan`), so it never frees.
// Its own crate (`arc-lend`, which names nothing of tutti's); re-exported here
// so the engine spells it `tutti_rt::ArcLend`.
pub use arc_lend::{ArcLend, Loan, Reader};
