//! Real-time-callback primitives — the types the audio thread touches.
//!
//! Interior mutability, fixed-capacity buffers, and the denormals guard: each
//! is `#[no_alloc]`-safe on the audio path and carries no engine dependency.
//!
//! # How the buffers relate
//!
//! The three capped collections are **one policy plus three access
//! disciplines**, not three separate designs. A private `Capped<T, N>` answers
//! "what happens at the cap?" once — refuse the write, latch an overflow flag,
//! never grow — and each public type composes it with exactly one answer to
//! "who may reach this, and how do they read it back?":
//!
//! | type | reached through | reads back via |
//! |---|---|---|
//! | [`RtVec`] | `&mut self` | `as_slice()` — a borrowed `&[T]` |
//! | [`RtEventBuf`] | `&self` (an [`AudioThreadCell`]) | `for_each` / `drain_each` |
//! | [`RtScratchBuf`] | `&self` (an `UnsafeCell`) | a `&self`-lifetime slice |
//!
//! Pick by how the owner holds the buffer: `&mut self` in hand means
//! [`RtVec`]; behind an `Arc` or a COM object means one of the other two, and
//! then by whether the filled run must outlive the call that produced it.
//!
//! [`RtScratch`] is deliberately outside that family: it has no `push` at all,
//! so there is no cap to enforce — its length is chosen per block by slicing a
//! preallocated run.
//!
//! - [`AudioThreadCell`] — interior mutability with a "one borrow at a time"
//!   contract, reached through `&self` (e.g. behind an `Arc` or a COM object).
//! - [`RtEventBuf`] — a fixed-inline-capacity event collector refilled each
//!   block, built on [`AudioThreadCell`]. Read it with `for_each`, or consume
//!   it with `drain_each` when the callback needs `&mut` access to whatever
//!   owns the collector.
//! - [`RtScratchBuf`] — fill-then-lend: refills each block and lends its filled
//!   slice out with `&self` lifetime — the one "return a borrow back to the
//!   caller" shape [`AudioThreadCell`] cannot give. Its fill closure receives a
//!   [`CappedWriter`], so overflow is refused rather than heap-allocated.
//! - [`RtScratch`] — a fixed-*capacity* scratch buffer with no grow/push API;
//!   the active length per block is chosen by slicing, not by resizing. Sibling
//!   to [`RtScratchBuf`] with a different contract (own-and-slice vs lend).
//! - [`RtVec`] — collect-then-lend through `&mut self`: a capped collection
//!   that owns its storage and exposes it as `&[T]`. What a per-block pool
//!   wants when its owner already has `&mut self`, and the shape the others
//!   cannot serve — [`RtEventBuf`] hides its storage, [`RtScratchBuf`] lends
//!   through `&self` + `unsafe`, [`RtScratch`] has no `push`.
//! - [`RtPublish`] — a value published from a control thread and read by the
//!   audio thread, where the read is a *borrow*: the callback never holds an
//!   owning handle, so retired values are freed by the publisher.
//! - [`ScopedNoDenormals`] — RAII guard that flushes subnormals to zero for the
//!   duration of an audio block, then restores the FPU control register.

mod capped;
pub mod cell;
pub mod denormals;
pub mod event_buf;
pub mod publish;
pub mod scratch;
pub mod scratch_buf;
pub mod vec;

pub use cell::{AudioThreadCell, BorrowGuard, BorrowRef};
pub use denormals::ScopedNoDenormals;
pub use event_buf::RtEventBuf;
pub use publish::{RtPublish, RtRef};
pub use scratch::{RtScratch, RtScratchOverflow};
pub use scratch_buf::{CappedWriter, RtScratchBuf};
pub use vec::RtVec;
