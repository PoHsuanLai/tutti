//! Shared real-time primitives for the Tutti audio engine.
//!
//! These types are the common vocabulary for code reached from the audio
//! callback, factored into one `no_std` crate so `tutti-core` and the plugin
//! host crates share a single definition rather than duplicating it.
//!
//! - [`AudioThreadCell`]: interior mutability with a "one borrow at a time"
//!   contract, reached through `&self` (e.g. behind an `Arc` or a COM object).
//! - [`RtEventBuf`]: a fixed-inline-capacity event collector refilled each
//!   block, built on [`AudioThreadCell`].

#![no_std]

mod audio_thread_cell;
mod rt_event_buf;

pub use audio_thread_cell::{AudioThreadCell, BorrowGuard, BorrowRef};
pub use rt_event_buf::RtEventBuf;
