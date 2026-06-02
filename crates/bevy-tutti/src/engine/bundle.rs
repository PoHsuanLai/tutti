//! Top-level [`TuttiEngine`] — a flat owning bundle returned from the builder.
//!
//! `TuttiEngine` itself has no behavior; it just holds the subsystems so callers
//! can destructure and drive them directly:
//!
//! ```ignore
//! let engine = TuttiEngine::builder().build()?;
//! let TuttiEngine { mut graph, mut driver, transport, metering, .. } = engine;
//!
//! let id = graph.add(sine_hz(440.0));
//! graph.pipe_output(id);
//! graph.commit();
//! transport.play();
//! ```
//!
//! Each field is `Clone + Send + Sync` (or an owning type that exposes `Clone`
//! handles), which lets Bevy wrap them as individual `Resource`s and schedule
//! edit-systems (`ResMut<TuttiGraphRes>`) independently from read systems
//! (`Res<TransportRes>`, `Res<MeteringRes>`).

use tutti_core::MeteringHandle;
use crate::engine::{TuttiDriver, TuttiGraph};
use tutti_core::processor::GraphProcessor;
use tutti_core::TransportHandle;

#[cfg(feature = "midi")]
use tutti_midi_io::MidiIo;
#[cfg(feature = "midi")]
use tutti_core::midi::MidiProcessor;
#[cfg(feature = "midi")]
use tutti_midi_runtime::MidiBus;

#[cfg(feature = "sampler")]
use tutti_sampler::Sampler;
#[cfg(feature = "sampler")]
use tutti_core::Arc;

#[cfg(feature = "soundfont")]
use tutti_core::Arc as _SoundFontArc;
#[cfg(feature = "soundfont")]
use tutti_synth::SoundFontSystem;

#[cfg(feature = "analysis")]
use tutti_analysis::AnalysisHandle;

/// The audio processor type that runs on the RT callback thread.
///
/// - With `midi`: `MidiProcessor<GraphProcessor>` — splits buffers on MIDI
///   events, routes them through the caller-supplied [`MidiQueue`], then
///   ticks the graph.
/// - Without `midi`: `GraphProcessor` — just ticks the graph.
///
/// [`MidiQueue`]: tutti_core::midi::MidiQueue
#[cfg(feature = "midi")]
pub type DefaultProcessor = MidiProcessor<GraphProcessor>;
#[cfg(not(feature = "midi"))]
pub type DefaultProcessor = GraphProcessor;

/// Flat owning bundle of all audio subsystems.
///
/// Constructed via [`TuttiEngine::builder`]. Callers destructure this into
/// individual fields; there are no methods beyond `builder()`.
pub struct TuttiEngine {
    /// Editable DSP graph. `&mut self` edits; call `graph.commit()` to publish.
    pub graph: TuttiGraph,

    /// CPAL I/O lifecycle. `&mut self` to change device / restart.
    pub driver: TuttiDriver,

    /// Lock-free transport control (play/stop/seek/tempo/loop). `Clone`.
    pub transport: TransportHandle,

    /// Lock-free metering snapshots. `Clone`.
    pub metering: MeteringHandle,

    /// Sample rate as reported by the audio device.
    pub sample_rate: f64,

    /// Channel count configured for the output bus.
    pub channels: usize,

    /// Audio-thread MIDI fan-out: dispatches events to the right node inbox
    /// by [`MidiUnitId`](tutti_midi_types::MidiUnitId). `Clone`.
    #[cfg(feature = "midi")]
    pub midi: MidiBus,

    /// OS MIDI port manager. `None` unless `.midi()` was called on the builder.
    #[cfg(feature = "midi")]
    pub midi_io: Option<MidiIo>,

    /// Sampler subsystem (disk streaming, clip playback, capture).
    #[cfg(feature = "sampler")]
    pub sampler: Arc<Sampler>,

    /// SoundFont system (file cache + synth instantiation).
    #[cfg(feature = "soundfont")]
    pub soundfont: _SoundFontArc<SoundFontSystem>,

    /// Analysis handle (transient / pitch / stereo analysis). `Clone`.
    ///
    /// Offers on-demand analysis (`detect_pitch`, `detect_transients`, …) and,
    /// when constructed with live state, cached snapshots via `live_pitch` /
    /// `live_transients` / `live_waveform` / `live_spectrum`.
    #[cfg(feature = "analysis")]
    pub analysis: AnalysisHandle,
}

impl TuttiEngine {
    /// Start a new builder.
    pub fn builder() -> crate::engine::TuttiEngineBuilder {
        crate::engine::TuttiEngineBuilder::default()
    }
}
