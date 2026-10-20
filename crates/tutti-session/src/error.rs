//! The session's aggregate error.

use thiserror::Error;

/// Every subsystem's error, so `?` propagates across crate boundaries.
///
/// Each variant wraps a subsystem error via `#[from]`. Device failures arrive
/// as one `Device` variant (feature `device`) rather than a per-CPAL-call
/// spread: `tutti_cpal::Error` owns that distinction.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// Wraps an error from `tutti_core::Error`.
    #[error(transparent)]
    Core(#[from] tutti_core::Error),

    /// The graph engine refused its graph: wraps
    /// `tutti_core::GraphEngineError`.
    #[error("graph engine: {0}")]
    Graph(#[from] tutti_core::GraphEngineError),

    /// The graph refused a re-prepare: a device restart asked for a block
    /// past the engine's capacity, came while another re-prepare was between
    /// its halves, or met a graph that no longer compiles at the new block.
    /// Wraps `tutti_graph::CommitError`.
    #[error("graph re-prepare: {0}")]
    Reprepare(#[from] tutti_graph::CommitError),

    /// Audio device failure: enumeration, stream construction, or playback.
    /// Wraps `tutti_cpal::Error`, which owns every CPAL concern.
    #[cfg(feature = "device")]
    #[error(transparent)]
    Device(#[from] tutti_cpal::Error),

    /// MIDI subsystem failure. Wraps `tutti_midi_hardware::Error`, and is
    /// therefore gated on **`midi-hardware`** rather than `midi`: the error
    /// type comes from the OS port layer, and the software-only build has no
    /// fallible MIDI step.
    #[cfg(feature = "midi-hardware")]
    #[error("MIDI: {0}")]
    Midi(#[from] tutti_midi_hardware::Error),

    /// A MIDI call named a target that is not in the graph, or takes no
    /// MIDI (it has no event input): `Midi::play_clip`, `Midi::live_input`,
    /// `Midi::retarget`.
    #[cfg(feature = "midi")]
    #[error("{0:?} takes no MIDI: it is not in the graph, or has no event input")]
    NoEventInput(tutti_core::AudioNode),

    /// A route rule on a session with no MIDI input node (one built
    /// detached).
    #[cfg(feature = "midi")]
    #[error("the session has no MIDI input node to route")]
    NoMidiInput,

    /// `Midi::retarget` named a node that is not a clip or live input of
    /// the session's.
    #[cfg(feature = "midi")]
    #[error("{0:?} is not a clip or live input of this session's")]
    NotSessionMidi(tutti_core::AudioNode),

    /// The hardware input's routes have one declarer: the session's own
    /// (`Midi::route`, `Midi::set_route_fallback`) or a host's
    /// (`Midi::set_routes`), not both at once.
    #[cfg(feature = "midi")]
    #[error(
        "the MIDI routes are declared by {0}; clear those before declaring \
         them the other way"
    )]
    TwoRouteDeclarers(&'static str),

    /// Polyphonic-synth subsystem failure. Wraps `tutti_polysynth::Error`.
    #[cfg(feature = "synth")]
    #[error("Synth: {0}")]
    Synth(#[from] tutti_polysynth::Error),

    /// SoundFont subsystem failure. Wraps `tutti_soundfont::Error`; a
    /// separate variant from [`Self::Synth`] because the two are separate
    /// crates with disjoint failure modes.
    #[cfg(feature = "soundfont")]
    #[error("SoundFont: {0}")]
    SoundFont(#[from] tutti_soundfont::Error),

    /// Sampler subsystem failure. Wraps `tutti_sampler::Error`.
    #[cfg(feature = "sampler")]
    #[error("Sampler: {0}")]
    Sampler(#[from] tutti_sampler::Error),

    /// Spatial-audio subsystem failure. Wraps `tutti_spatial::VbapError`
    /// (VBAP speaker-layout construction). HRTF has its own error type and
    /// does not route through here.
    #[cfg(feature = "spatial")]
    #[error("Spatial: {0}")]
    Spatial(#[from] tutti_spatial::VbapError),

    /// Plugin-host bridge failure (VST2/VST3/CLAP). Wraps
    /// `tutti_plugin::BridgeError`.
    #[cfg(feature = "plugin")]
    #[error("Plugin: {0}")]
    Plugin(#[from] tutti_plugin::BridgeError),

    /// Offline rendering / export subsystem failure. Wraps
    /// `tutti_export::Error`.
    #[cfg(feature = "export")]
    #[error("Export: {0}")]
    Export(#[from] tutti_export::Error),

    /// Another export is in flight: its job, or what the job returned, is
    /// still alive (`Session::export`). Finish or drop it first.
    #[cfg(feature = "export")]
    #[error("another export is in flight")]
    ExportInFlight,

    /// Standard I/O error encountered while reading or writing audio data.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// `Result<T, `[`enum@Error`]`>`.
pub type Result<T> = core::result::Result<T, Error>;
