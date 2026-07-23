//! Real-time-callback primitives — the types the audio thread touches.
//!
//! Interior mutability, fixed-capacity buffers, and the denormals guard: each
//! is `#[no_alloc]`-safe on the audio path and carries no engine dependency.
//!
//! - [`AudioThreadCell`] — interior mutability with a "one borrow at a time"
//!   contract, reached through `&self` (e.g. behind an `Arc` or a COM object).
//! - [`RtEventBuf`] — a fixed-inline-capacity event collector refilled each
//!   block, built on [`AudioThreadCell`].
//! - [`RtScratchBuf`] — fill-then-lend: refills each block and lends its filled
//!   slice out with `&self` lifetime — the one "return a borrow back to the
//!   caller" shape [`AudioThreadCell`] cannot give.
//! - [`RtScratch`] — a fixed-*capacity* scratch buffer with no grow/push API;
//!   the active length per block is chosen by slicing, not by resizing. Sibling
//!   to [`RtScratchBuf`] with a different contract (own-and-slice vs lend).
//! - [`ScopedNoDenormals`] — RAII guard that flushes subnormals to zero for the
//!   duration of an audio block, then restores the FPU control register.

pub mod cell;
pub mod denormals;
pub mod event_buf;
pub mod scratch;
pub mod scratch_buf;

pub use cell::{AudioThreadCell, BorrowGuard, BorrowRef};
pub use denormals::ScopedNoDenormals;
pub use event_buf::RtEventBuf;
pub use scratch::{RtScratch, RtScratchOverflow};
pub use scratch_buf::RtScratchBuf;
