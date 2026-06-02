//! The Tutti audio engine, owned by bevy-tutti.
//!
//! Relocated from the former standalone `tutti` umbrella crate when bevy-tutti
//! became the umbrella. Holds the CPAL audio callback (`audio_io`), the device
//! lifecycle (`driver`), the editable DSP graph (`graph`), the engine bootstrap
//! (`builder`), and the flat `TuttiEngine` bundle (`bundle`). `TuttiPlugin`
//! builds a `TuttiEngine` and destructures it into Bevy resources.
//!
//! Unlike the old umbrella these are NOT gated on a `std` feature — bevy-tutti
//! always runs with std (it drives a Bevy `App` over CPAL).

mod audio_io;
mod builder;
mod bundle;
mod driver;
mod error;
mod graph;

#[cfg(all(feature = "midi", feature = "export"))]
pub mod midi_export;

pub use builder::TuttiEngineBuilder;
pub use bundle::{DefaultProcessor, TuttiEngine};
pub use driver::{DeviceInfo, TuttiDriver};
pub use error::{Error, Result};
pub use graph::TuttiGraph;
