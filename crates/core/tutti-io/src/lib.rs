#![doc = include_str!("../README.md")]

mod node_id;

mod codec;
#[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
mod decode;
mod error;
#[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
mod file_in;
mod mic;
mod recorder;
mod tap_in;
mod wav_out;
mod wave;

pub use error::{Error, Result};
pub use mic::{share_mic_ring, MicMonitorNode, MicRing};
pub use recorder::{
    FinalizeStatus, ManualDriver, ManualPump, PumpDriver, PumpLoop, PumpPass, Recorder,
    RunningPump, ThreadDriver,
};
pub use tap_in::TapIn;
// `MAX_WAV_FOLD_CHANNELS` is the widest input `WavOut` will fold, so a caller
// sizing a buffer for it has to name the same ceiling.
pub use wav_out::{WavOut, MAX_WAV_FOLD_CHANNELS};

// The file-reading half of the edge, moved out of the fundsp fork (design doc
// 013, Phase 0). `Wave` is the resident buffer and needs no codec; decoding
// into it, probing a header and streaming (`FileIn`) exist only when a codec
// feature compiled symphonia in. `decodable_extensions` answers for whichever
// features did, and is empty when none did.
pub use codec::{can_decode, decodable_extensions};
#[cfg(all(
    feature = "bevy",
    any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg")
))]
pub use decode::WaveAsset;
#[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
pub use decode::{WaveError, WaveMetadata};
#[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
pub use file_in::FileIn;
pub use wave::Wave;

// The traits this crate implements, re-exported so a consumer reaches the
// vocabulary and its live impls from one place.
pub use tutti_core::io::{pump, AudioIn, AudioOut, OnEmpty};
pub use tutti_core::pcm::BitDepth;
