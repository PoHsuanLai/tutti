//! FFT-based convolution reverb.
//!
//! A uniformly partitioned FFT convolution ([`Convolver`], over an IR's
//! shared [`IrSpectra`]) as a graph node ([`ConvolverNode`]), so a
//! convolution reverb lives in a graph alongside the built-in delay,
//! modulation, dynamics, and spatial nodes.
//!
//! IR samples are supplied by the caller — usually via `tutti-sampler`,
//! which already owns WAV / FLAC / MP3 / OGG loading. This module is
//! deliberately format-agnostic.

mod convolver;
mod ir;
mod node;
mod params;

pub use convolver::{Convolver, IrSpectra};
pub use ir::{generate_room_ir, generate_room_ir_into, generate_test_ir, generate_test_ir_into};
pub use node::{ConvolverNode, IrChannelConfig};
pub use params::WetDry;
