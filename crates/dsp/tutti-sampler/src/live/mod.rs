//! The live audio edge: what comes in from a device, and what goes out to a
//! file.
//!
//! [`mic`] is the read side — a monitoring node over a device-filled ring; the
//! device layer itself lives in `tutti-cpal`. [`wav_out`] is the write side, an
//! [`AudioOut`](tutti_core::io::AudioOut) sink. Recording is a
//! [`pump`](tutti_core::io::pump) from one to the other.
//!
//! Neither touches voice playback or the butler thread. `wav_out` in particular
//! sat under `butler/io/` with no butler coupling of any kind.

pub mod mic;
pub mod wav_out;

pub use mic::{share_mic_ring, MicMonitorNode, MicRing};
pub use wav_out::{CaptureFormat, WavOut};
