//! Plugin lifecycle types: [`Vst3Library`] (loaded DSO + factory),
//! [`Vst3Loaded`] (initialized plugin, no audio), and [`Vst3Instance`] (active,
//! ready to `process`). Stages are encoded as distinct types — transitions
//! consume `self` so the compiler enforces the ordering.

mod instance;
mod library;
mod loaded;
mod midi_mapping;

pub use instance::Vst3Instance;
pub use library::Vst3Library;
pub use loaded::{RestartOutcome, Vst3Loaded};
