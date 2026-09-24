//! FFT-based convolution reverb.
//!
//! Wraps [`fft_convolver::FFTConvolver`] in a `tutti-core` [`AudioUnit`],
//! so a convolution reverb can live inside a `NodeNetwork` alongside
//! the built-in delay, modulation, dynamics, and spatial nodes.
//!
//! IR samples are supplied by the caller — usually via `tutti-sampler`,
//! which already owns WAV / FLAC / MP3 / OGG loading. This module is
//! deliberately format-agnostic.
//!
//! [`AudioUnit`]: tutti_core::AudioUnit

mod convolver;
mod ir;
mod node;
mod params;

pub use convolver::Convolver;
pub use ir::{generate_room_ir, generate_room_ir_into, generate_test_ir, generate_test_ir_into};
pub use node::{ConvolverNode, IrChannelConfig};
pub use params::WetDry;
