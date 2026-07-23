//! Shared vocabulary for the Tutti audio engine.
//!
//! The common definitions every subsystem — `tutti-core`, the plugin host
//! crates, export, analysis — shares rather than duplicating. Two families:
//!
//! **RT-callback primitives** (reached from the audio thread):
//! - [`AudioThreadCell`]: interior mutability with a "one borrow at a time"
//!   contract, reached through `&self` (e.g. behind an `Arc` or a COM object).
//! - [`RtEventBuf`]: a fixed-inline-capacity event collector refilled each
//!   block, built on [`AudioThreadCell`].
//! - [`RtScratchBuf`]: a fixed-inline-capacity scratch buffer that refills each
//!   block and lends its filled slice out with `&self` lifetime — the one
//!   "return a borrow back to the caller" shape `AudioThreadCell` cannot give.
//!
//! **The I/O edge vocabulary** ([`io`]): [`AudioIn`] / [`AudioOut`] — the two
//! traits every audio source and sink in the engine speaks (mic, file, disk,
//! plugin boundary), plus [`pump`](io::pump). Homed here, at the root leaf, so
//! every subsystem can implement them without an absurd dependency edge.
//!
//! **Latency compensation** ([`latency`]): the [`LatencyGraph`] trait and the
//! [`plan`](latency::plan) / [`compensate`] algorithm that aligns unequal signal
//! paths, plus the [`Samples`] count it speaks ([`units`]). Pure graph math with
//! no audio dependency, so any graph representation can drive it.

mod audio_thread_cell;
mod rt_event_buf;
mod rt_scratch_buf;

pub mod io;
pub mod latency;
pub mod units;

pub use audio_thread_cell::{AudioThreadCell, BorrowGuard, BorrowRef};
pub use io::{pump, AudioIn, AudioOut};
pub use latency::{compensate, Compensation, DelayInsertion, LatencyGraph};
pub use rt_event_buf::RtEventBuf;
pub use rt_scratch_buf::RtScratchBuf;
pub use units::Samples;
