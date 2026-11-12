//! The session's aggregate error.

use thiserror::Error;

/// Why [`Session::open_heard_tap`](crate::Session::open_heard_tap) or
/// [`Session::close_heard_tap`](crate::Session::close_heard_tap) refused.
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
    /// No heard tap is open to close (none was opened, it was closed, or its
    /// dropped reader was reclaimed).
    #[error("no heard tap is open")]
    NotOpen,
}

/// Why [`Graph::remove`](crate::Graph::remove) (or
/// [`Session::remove`](crate::Session::remove)) refused: what was not there.
/// Nothing is changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum RemoveError {
    /// The node is not in the graph: removed already, or never added.
    #[error("{0:?} is not in the graph")]
    NoSuchNode(tutti_graph::NodeKey),
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

/// Why [`Graph::connect`](crate::Graph::connect),
/// [`Graph::set_sources`](crate::Graph::set_sources) or
/// [`Graph::chain`](crate::Graph::chain) refused; nothing is stored.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ConnectError {
    /// A node is not in the graph.
    #[error("{0:?} is not in the graph")]
    NoSuchNode(tutti_graph::NodeKey),
    /// An output port that the node (or the graph's input) does not have.
    #[error("no such output port: {0:?}")]
    NoSuchOutPort(crate::OutPort),
    /// An input port that the node (or the graph's output) does not have.
    #[error("no such input port: {0:?}")]
    NoSuchInPort(crate::InPort),
    /// An event output that the node does not have.
    #[error("no such event output: {0:?}")]
    NoSuchEventOut(crate::EventOutPort),
    /// An event input that the node does not have.
    #[error("no such event input: {0:?}")]
    NoSuchEventIn(crate::EventInPort),
    /// A node feeds itself: a node cannot be its own source.
    #[error("{0:?} cannot feed itself")]
    Itself(tutti_graph::NodeKey),
    /// The source declares no main output bus, or the sink no main input
    /// bus, to connect channel by channel.
    #[error("{0:?} has no main bus on that side")]
    NoMainBus(tutti_graph::NodeKey),
    /// The two main buses are not the same width. A connection never
    /// truncates or zero-fills: a width change is a bus edge
    /// ([`Graph::connect`](crate::Graph::connect)).
    #[error(
        "{from:?}'s main output is {from_width} wide and {to:?}'s main input {to_width}; \
         a width change is a bus edge"
    )]
    Width {
        /// The source node.
        from: tutti_graph::NodeKey,
        /// Its main output's width.
        from_width: usize,
        /// The sink node.
        to: tutti_graph::NodeKey,
        /// Its main input's width.
        to_width: usize,
    },
    /// A main bus is wider than the root it connects to: a connection never
    /// truncates.
    #[error(
        "{from:?}'s main output is {from_width} wide and the root {root_width}; \
         a width change is a bus edge"
    )]
    RootNarrow {
        /// The source node.
        from: tutti_graph::NodeKey,
        /// Its main output's width.
        from_width: usize,
        /// The root's width.
        root_width: usize,
    },
    /// A bus edge was refused.
    #[error(transparent)]
    Bus(#[from] tutti_graph::BusError),
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

    /// A modulation declaration refused when it was made: wraps
    /// [`RouteError`](crate::RouteError).
    #[cfg(feature = "modulation")]
    #[error(transparent)]
    Modulation(#[from] crate::RouteError),

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

    /// A whole-bus connection was refused ([`Graph::connect`](crate::Graph::connect)).
    #[error(transparent)]
    Connect(#[from] ConnectError),

    /// A param write was refused ([`Graph::set_param`](crate::Graph::set_param)).
    #[error(transparent)]
    Param(#[from] crate::ParamError),

    /// A modulation range was refused (`Mods::range`).
    #[error(transparent)]
    Range(#[from] crate::RangeError),

    /// A bus edge was refused ([`Graph::connect`](crate::Graph::connect)).
    #[error(transparent)]
    Bus(#[from] tutti_graph::BusError),

    /// A node mode was refused ([`Graph::set_mode`](crate::Graph::set_mode)).
    #[error(transparent)]
    Mode(#[from] tutti_graph::ModeError),

    /// A dry/wet mix was refused ([`Graph::set_mix`](crate::Graph::set_mix)).
    #[error(transparent)]
    Mix(#[from] tutti_graph::MixError),

    /// The transport's command queue was full ([`Session::play`](crate::Session::play)
    /// and the other verbs).
    #[error(transparent)]
    Transport(#[from] tutti_transport::QueueFull),

    /// The transport's timed-command list was full
    /// ([`Session::schedule`](crate::Session::schedule)).
    #[error(transparent)]
    Schedule(#[from] tutti_transport::ScheduleFull),

    /// A file could not be decoded (`Wave::read`, `FileIn`).
    #[error(transparent)]
    Decode(#[from] tutti_codec::DecodeError),

    /// A wave cannot be placed into a sampler voice's channels.
    #[cfg(feature = "sampler")]
    #[error(transparent)]
    Place(#[from] tutti_sampler::PlaceError),

    /// A bounce could not start or finish (a cancel included:
    /// [`BounceError::Cancelled`](crate::BounceError::Cancelled)).
    #[cfg(feature = "export")]
    #[error(transparent)]
    Bounce(#[from] crate::BounceError),

    /// A bounce asks for something no render could give.
    #[cfg(feature = "export")]
    #[error(transparent)]
    BounceConfig(#[from] crate::BounceConfigError),

    /// Standard I/O error encountered while reading or writing audio data.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// `Result<T, `[`enum@Error`]`>`.
pub type Result<T> = core::result::Result<T, Error>;
