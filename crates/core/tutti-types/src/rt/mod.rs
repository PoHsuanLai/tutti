//! Real-time-callback primitives — the types the audio thread touches.
//!
//! Interior mutability, fixed-capacity buffers, and the denormals guard: each
//! is `#[no_alloc]`-safe on the audio path and carries no engine dependency.
//!
//! # How the buffers relate
//!
//! The two capped collections are **one policy plus two access disciplines**,
//! not two separate designs. A private `Capped<T, N>` answers "what happens at
//! the cap?" once — refuse the write, latch an overflow flag, never grow — and
//! each public type composes it with exactly one answer to "who may reach this,
//! and how do they read it back?":
//!
//! | type | reached through | reads back via |
//! |---|---|---|
//! | [`RtVec`] | `&mut self` | `as_slice()` — a borrowed `&[T]` |
//! | [`RtEventBuf`] | `&self` (an [`AudioThreadCell`]) | `for_each` / `drain_each` |
//!
//! Pick by how the owner holds the buffer: `&mut self` in hand means [`RtVec`];
//! behind an `Arc` (a CPAL callback's shared state, a COM object) means
//! [`RtEventBuf`], which pays for that reach with a scoped borrow guard.
//!
//! Neither lends a borrow into its storage out of a `&self` method. That shape
//! needs an `UnsafeCell` and a single-thread contract the compiler cannot
//! check, and no site in the engine requires it: a consumer that wants the
//! events either owns the buffer (`&mut self`) or takes them through a visitor.
//!
//! [`RtScratch`] is deliberately outside the family: it has no `push` at all,
//! so there is no cap to enforce — its length is chosen per block by slicing a
//! preallocated run.
//!
//! - [`AudioThreadCell`] — interior mutability with a "one borrow at a time"
//!   contract, reached through `&self` (e.g. behind an `Arc` or a COM object).
//! - [`RtEventBuf`] — a fixed-inline-capacity event collector refilled each
//!   block, built on [`AudioThreadCell`]. Read it with `for_each`, or consume
//!   it with `drain_each` when the callback needs `&mut` access to whatever
//!   owns the collector.
//! - [`RtScratch`] — a fixed-*capacity* scratch buffer with no grow/push API;
//!   the active length per block is chosen by slicing, not by resizing.
//! - [`RtVec`] — collect-then-lend through `&mut self`: a capped collection
//!   that owns its storage and exposes it as `&[T]`. What a per-block pool
//!   wants when its owner already has `&mut self`.
//! - [`RtPublish`] — a value published from a control thread and read by the
//!   audio thread, where the read is a *borrow*: the callback never holds an
//!   owning handle, so retired values are freed by the publisher.
//! - [`PosRing`] — frames a writer thread places ahead of the audio thread,
//!   indexed by position rather than consumed in order (a disk stream's ring):
//!   one [`PosWriter`] and one [`PosReader`], whose [`PosClaim`] reads any
//!   position the window it took holds.
//! - [`Retire`] — an owning box that must not be dropped on the audio thread,
//!   checked in debug builds against the [`AudioThread`] marker.
//! - [`ScopedNoDenormals`] — RAII guard that flushes subnormals to zero for the
//!   duration of an audio block, then restores the FPU control register.

pub mod audio_thread;
mod capped;
pub mod cell;
pub mod denormals;
pub mod event_buf;
pub mod pos_ring;
pub mod publish;
pub mod retire;
pub mod scratch;
pub mod vec;

pub use audio_thread::{AudioThread, AudioThreadGuard};
pub use cell::{AudioThreadCell, BorrowGuard, BorrowRef};
pub use denormals::ScopedNoDenormals;
pub use event_buf::RtEventBuf;
pub use pos_ring::{
    PosClaim, PosFrame, PosReader, PosRing, PosWriter, RingWindow, MAX_POS_RING_FRAMES,
};
pub use publish::{RtPublish, RtRef};
pub use retire::Retire;
pub use scratch::{RtScratch, RtScratchOverflow};
pub use vec::RtVec;
