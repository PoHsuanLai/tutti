//! The session's aggregate error.

use thiserror::Error;

/// Why [`Session::open_heard_tap`](crate::Session::open_heard_tap) refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum HeardTapError {
    /// The session has no engine, so nothing renders to tap (a detached
    /// session; a plan tap, [`Graph::open_tap`](crate::Graph::open_tap), is
    /// fed by its `render_frame`).
    #[error("this session has no engine to tap")]
    Detached,
    /// A reader of the output tap is alive; close it first.
    #[error("the output tap already has a reader; close it before opening again")]
    Busy,
    /// The audio thread has not yet taken the last installs.
    #[error("the audio thread has not yet taken the last tap installs")]
    Backpressure,
}

/// Why [`Graph::open_inlet`](crate::Graph::open_inlet) refused.
#[derive(Clone, Copy, Debug, PartialEq, Error)]
#[non_exhaustive]
pub enum InletError {
    /// The producer runs at another rate than the graph. An inlet does not
    /// resample, so the signal would play at the wrong speed (and drift
    /// besides); open the producer at the graph's rate.
    #[error(
        "the inlet's producer runs at {producer:?} but the graph at {graph:?}; \
         an inlet does not resample"
    )]
    RateMismatch {
        /// The rate the producer fills the ring at.
        producer: tutti_types::SampleRate,
        /// The graph's rate when the inlet was opened.
        graph: tutti_types::SampleRate,
    },
}

/// Every subsystem's error, so `?` propagates across crate boundaries.
///
/// Each variant wraps a subsystem error via `#[from]`. Device failures arrive
/// as one `Device` variant (feature `device`) rather than a per-CPAL-call
/// spread: `tutti_cpal::Error` owns that distinction.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// Wraps an error from `tutti_engine::Error`.
    #[error(transparent)]
    Engine(#[from] tutti_engine::Error),

    /// The graph engine refused its graph: wraps
    /// `tutti_engine::GraphEngineError`.
    #[error("graph engine: {0}")]
    Graph(#[from] tutti_engine::GraphEngineError),

    /// The graph root cannot be placed on the output: the device has no
    /// channels, or the root is wider than a device at least one of whose
    /// widths has no channel order (a 6-wide root on a 5-channel device would
    /// lose a channel). Wraps `tutti_types::FoldError`.
    #[error("the graph root cannot be placed on the output: {0}")]
    Output(#[from] tutti_types::FoldError),

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
    NoEventInput(tutti_graph::NodeKey),

    /// [`Session::instrument`](crate::Session::instrument) (or
    /// `add_instrument`, `instrument_plugin`) was given a node that cannot be
    /// played as an instrument.
    #[cfg(feature = "midi")]
    #[error("{node:?} is not an instrument: {why}")]
    NotInstrument {
        /// The node.
        node: tutti_graph::NodeKey,
        /// Why not.
        why: crate::NotInstrument,
    },

    /// A route rule on a session with no MIDI input node (one built
    /// detached).
    #[cfg(feature = "midi")]
    #[error("the session has no MIDI input node to route")]
    NoMidiInput,

    /// `Midi::retarget` named a node that is not a clip or live input of
    /// the session's.
    #[cfg(feature = "midi")]
    #[error("{0:?} is not a clip or live input of this session's")]
    NotSessionMidi(tutti_graph::NodeKey),

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
    /// separate variant from `Synth` because the two are separate
    /// crates with disjoint failure modes.
    #[cfg(feature = "soundfont")]
    #[error("SoundFont: {0}")]
    SoundFont(#[from] tutti_soundfont::Error),

    /// Sampler subsystem failure. Wraps `tutti_sampler::Error`.
    #[cfg(feature = "sampler")]
    #[error("Sampler: {0}")]
    Sampler(#[from] tutti_sampler::Error),

    /// Spatial-audio subsystem failure. Wraps `tutti_nodes::VbapError`
    /// (VBAP speaker-layout construction). HRTF has its own error type and
    /// does not route through here.
    #[error("Spatial: {0}")]
    Spatial(#[from] tutti_nodes::VbapError),

    /// Plugin-host bridge failure (VST2/VST3/CLAP). Wraps
    /// `tutti_plugin_types::BridgeError`.
    #[cfg(feature = "plugin")]
    #[error("Plugin: {0}")]
    Plugin(#[from] tutti_plugin_types::BridgeError),

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
