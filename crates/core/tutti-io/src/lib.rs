//! Tutti's live audio I/O edge: what comes in from a device, and what goes out
//! to a file.
//!
//! [`MicMonitorNode`] is the read side — a monitoring node over a device-filled ring.
//! [`WavOut`] is the write side, an [`AudioOut`]
//! sink. Recording is a [`pump`] from one to the other,
//! which is all [`Recorder`] is.
//!
//! [`TapIn`] is the *other* read side: the analysis tap's consumer end as an
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
//!
//! # Example — recording what the graph is playing
//!
//! The tap path end to end, and it needs no device: `tutti-core`'s `AudioTap`
//! is pushed by the audio callback, [`TapIn`] is its consumer end as an
//! [`AudioIn`], and [`pump`] moves one block into a [`WavOut`]. Swap [`TapIn`]
//! for `tutti_cpal::MicIn` and the same three lines record a microphone —
//! that interchangeability is the reason the traits exist.
//!
//! ```
//! use tutti_core::AudioTap;
//! use tutti_io::{pump, AudioIn, AudioOut, BitDepth, TapIn, WavOut};
//!
//! let tap = AudioTap::new();
//! let mut src = TapIn::new(tap.open().expect("a fresh tap has no other reader"));
//!
//! // The callback's push is denominated in FRAMES; the slice it reads from is
//! // interleaved, so it must hold `frames * 2` samples.
//! let block = [0.25f32, -0.25, 0.5, -0.5, 0.75, -0.75];
//! tap.push(&block, 3);
//!
//! let dir = tempfile::tempdir().expect("temp dir");
//! let path = dir.path().join("take.wav");
//! let mut wav = WavOut::create(&path, 48_000.0, 2u16, BitDepth::Float32)
//!     .expect("sink opens");
//! assert_eq!(src.layout(), AudioOut::layout(&wav), "pump requires equal widths");
//!
//! // The scratch is sized in SAMPLES (`frames * channels`) because a flat
//! // interleaved slice has no other unit — but `pump` returns FRAMES. Conflating
//! // the two is this boundary's most repeated defect: a stereo take compared
//! // against a sample count runs half as long as it should.
//! let channels = src.layout().count() as usize;
//! let mut scratch = vec![0.0f32; 1024 * channels];
//! assert_eq!(pump(&mut src, &mut wav, &mut scratch), 3);
//!
//! // `finalize` takes `self`, so the header back-patch happens exactly once and
//! // writing after it is a compile error rather than a corrupt file.
//! wav.finalize().expect("header back-patches");
//! ```

mod node_id;

mod error;
mod mic;
mod recorder;
mod tap_in;
mod wav_out;

pub use error::{Error, Result};
pub use mic::{share_mic_ring, MicMonitorNode, MicRing};
pub use recorder::Recorder;
pub use tap_in::TapIn;
// `MAX_WAV_FOLD_CHANNELS` is the widest input `WavOut` will fold, so a caller
// sizing a buffer for it has to name the same ceiling.
pub use wav_out::{WavOut, MAX_WAV_FOLD_CHANNELS};

// The traits this crate implements, re-exported so a consumer reaches the
// vocabulary and its live impls from one place.
pub use tutti_core::io::{pump, AudioIn, AudioOut, OnEmpty};
pub use tutti_core::pcm::BitDepth;
