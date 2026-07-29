//! Tutti's live audio I/O edge: what comes in from a device, and what goes out
//! to a file.
//!
//! [`mic`] is the read side — a monitoring node over a device-filled ring.
//! [`wav_out`] is the write side, an [`AudioOut`]
//! sink. Recording is a [`pump`] from one to the other,
//! which is all [`Recorder`] is.
//!
//! [`tap_in`] is the *other* read side: the analysis tap's consumer end as an
//! [`AudioIn`], so what the graph is playing can be recorded by the same pump
//! that records a microphone. `AudioTap` itself belongs to `tutti-core` (the
//! audio callback pushes into it); this is only the adapter that lets it meet
//! the I/O vocabulary.
//!
//! # Where this sits
//!
//! ```text
//! tutti-types    the AudioIn/AudioOut traits, ChannelLayout, pcm quantizers
//!     ↑
//! tutti-core     Wave, AudioUnit; re-exports io
//!     ↑
//! tutti-io       MicMonitorNode, WavOut, Recorder      (device-free)
//!     ↑
//! tutti-cpal     MicIn, the output stream, the driver  (owns CPAL)
//! ```
//!
//! **Device-free on purpose.** [`MicMonitorNode`] is an `AudioUnit` that must be
//! able to sit anywhere in the graph, and [`WavOut`] has no device concern at
//! all. Folding these into `tutti-cpal` would make a headless render pull in
//! CPAL to write a WAV.
//!
//! The producer half of the mic ring lives in `tutti-cpal`'s input callback,
//! one layer up. [`Recorder`] likewise takes an already-open source rather than
//! opening a device, which is what lets it live here.

mod node_id;

pub mod mic;
pub mod recorder;
pub mod tap_in;
pub mod wav_out;

pub use mic::{share_mic_ring, MicMonitorNode, MicRing};
pub use recorder::Recorder;
pub use tap_in::TapIn;
pub use wav_out::WavOut;

// The traits this crate implements, re-exported so a consumer reaches the
// vocabulary and its live impls from one place.
pub use tutti_core::io::{pump, AudioIn, AudioOut, OnEmpty};
pub use tutti_core::pcm::BitDepth;
