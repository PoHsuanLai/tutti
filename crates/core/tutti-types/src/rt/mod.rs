//! Real-time-callback primitives — the types the audio thread touches.
//!
//! Interior mutability, fixed-capacity buffers, and the denormals guard: each
//! is `#[no_alloc]`-safe on the audio path and carries no engine dependency.
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
//! - [`RangeCell`] — a range's two `f32` bounds in one atomic word, so a
//!   reader never pairs one write's `min` with another's `max`.
//! - [`SeqWriter`] / [`SeqReader`] — a few words one writer (the audio
//!   thread, each block) overwrites in place and any thread copies whole: a
//!   sequence lock, for a value whose writer may not allocate or free. The
//!   writer is not `Clone`, so there is only one.
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
//! - [`ScopedNoDenormals`] — RAII guard that flushes subnormals to zero for the
//!   duration of an audio block, then restores the FPU control register.

pub mod cell;
pub mod denormals;
pub mod job_gate;
pub mod pos_ring;
pub mod range_cell;
pub mod scope;
pub mod seq_cell;
mod spin;
pub mod split;
pub mod task_graph;
pub mod vec;

pub use cell::{AudioThreadCell, BorrowGuard, BorrowRef};
pub use denormals::ScopedNoDenormals;
pub use job_gate::JobGate;
pub use pos_ring::{
    PosClaim, PosFrame, PosReader, PosRing, PosWriter, RingWindow, MAX_POS_RING_FRAMES,
};
pub use range_cell::RangeCell;
pub use scope::{RtScope, RtScopeGuard};
pub use seq_cell::{SeqReader, SeqWriter, SEQ_CELL_READ_RETRIES};
pub use split::{ClaimConflict, ClaimTable, Claimed, ClaimedRun, Held, RwView, SplitMut, SplitRw};
pub use task_graph::{Participant, TaskGraph};
pub use vec::RtVec;
