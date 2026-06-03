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

#[cfg(all(feature = "midi", feature = "export"))]
pub mod midi_export;

pub use builder::TuttiEngineBuilder;
pub use bundle::{DefaultProcessor, TuttiEngine};
pub use driver::{DeviceInfo, TuttiDriver};
pub use error::{Error, Result};
// `TuttiGraph` (plus `isolate_output` / `GraphDot`) moved into tutti-core; the
// engine surfaces them from there so existing `engine::TuttiGraph` paths hold.
pub use tutti_core::tutti_graph::{isolate_output, GraphDot};
pub use tutti_core::TuttiGraph;
