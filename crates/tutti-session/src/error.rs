//! The session's aggregate error.

use thiserror::Error;

/// Why [`Session::open_heard_tap`](crate::Session::open_heard_tap) or
/// [`Session::close_heard_tap`](crate::Session::close_heard_tap) refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Error)]
#[non_exhaustive]
pub enum HeardTapError {
    /// The session has no engine, so nothing renders to tap (a detached
    /// session; a plan tap, [`Taps::open`](crate::Taps::open), is
    /// fed by its `render_frame`).
    #[error("this session has no engine to tap")]
    Detached,
    /// A reader of the output tap is alive; close it first.
    #[error("the output tap already has a reader; close it before opening again")]
    Busy,
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

/// Why [`Session::schedule`](crate::Session::schedule) refused a transport
/// command. Nothing is sent.
#[derive(Clone, Copy, Debug, PartialEq, Error)]
#[non_exhaustive]
pub enum ScheduleError {
    /// The timed-command list was full; the command is handed back.
    #[error(transparent)]
    Full(#[from] tutti_transport::ScheduleFull),
    /// A command that starts the transport rolling was timed at a beat while
    /// the transport is stopped. A beat has a frame only once the transport
    /// rolls, so the command would wait for a playback that only it can
    /// start, and never fire. Start the transport first, or time it at a
    /// frame or the next block.
    #[error("{event:?} at {beat:?} cannot fire: a stopped transport never reaches a beat")]
    BeatWhileStopped {
        /// The beat it was timed at.
        beat: tutti_types::Beat,
        /// The command.
        event: tutti_transport::MotionEvent,
    },
}

/// The session refused to set the project tempo. Nothing is published.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Error)]
#[non_exhaustive]
pub enum TempoError {
    /// The session follows a live tempo (`Midi::follow_tempo`, feature
    /// `midi`), which owns the published map until `unfollow_tempo`: a map set now would be
    /// overwritten by the next live step, and unfollowing restores the map in
    /// force when following began.
    #[error("the session follows a live tempo; unfollow_tempo first")]
    Following,
}

/// A query or a control named a node the graph does not have: removed already,
/// or never added. Nothing is changed or read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[error("{0:?} is not in the graph")]
pub struct NoSuchNode(tutti_graph::NodeKey);

impl NoSuchNode {
    pub(crate) fn new(node: tutti_graph::NodeKey) -> Self {
        Self(node)
    }

    /// The key no node has.
    pub fn node(self) -> tutti_graph::NodeKey {
        self.0
    }
}

impl From<tutti_transport::ScheduleFull> for Error {
    fn from(full: tutti_transport::ScheduleFull) -> Self {
        Self::Schedule(full.into())
    }
}

/// The disk streamer's own failure (its thread would not start), as the
/// session's sampler error.
#[cfg(feature = "sampler")]
impl From<tutti_sampler::Error> for Error {
    fn from(error: tutti_sampler::Error) -> Self {
        Self::Sampler(crate::SamplerError::Streamer(error))
    }
}

impl From<NoSuchNode> for ConnectError {
    fn from(e: NoSuchNode) -> Self {
        Self::NoSuchNode(e.node())
    }
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

/// Why a MIDI call of the session failed (design 030, convention 2: one error
/// for the area, each cause carried as its source). `tutti::midi::Error` in
/// the umbrella; the session's own [`Error::Midi`] holds it, and every cause
/// below converts into both with `?`.
#[cfg(feature = "midi")]
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum MidiError {
    /// The OS port layer failed: enumeration, opening a device or its
    /// watch. Only a session with `midi-hardware` has a fallible MIDI step of
    /// this kind.
    #[cfg(feature = "midi-hardware")]
    #[error("MIDI hardware: {0}")]
    Hardware(#[from] tutti_midi_hardware::Error),

    /// A Profile switch was refused ([`Midi::set_profile`](crate::Midi::set_profile)).
    #[cfg(feature = "midi-hardware")]
    #[error("MIDI-CI Profile: {0}")]
    Profile(#[from] tutti_midi_hardware::ProfileError),

    /// A MIDI file could not be read, built or written.
    #[error("MIDI file: {0}")]
    File(#[from] tutti_midi_file::Error),

    /// A session job did not finish ([`Midi::load_file`](crate::Midi::load_file),
    /// `Midi::load_soundfont`): its work panicked, or the spawner dropped it.
    #[error("MIDI load: {0}")]
    Job(#[from] crate::JobFailed),

    /// A SoundFont file could not be read.
    #[cfg(feature = "soundfont")]
    #[error("SoundFont file: {0}")]
    SoundFont(#[from] tutti_sf2::Error),

    /// A SoundFont unit could not be built from the font and its settings.
    #[cfg(feature = "soundfont")]
    #[error("SoundFont instrument: {0}")]
    Instrument(#[from] tutti_soundfont::Error),

    /// A message was not queued ([`Refused::Full`](tutti_graph::Refused::Full)).
    #[error("MIDI send: {0}")]
    Send(#[from] tutti_graph::Refused),

    /// There is no such route rule ([`Midi::unroute`](crate::Midi::unroute)).
    #[error(transparent)]
    NoSuchRoute(#[from] crate::NoSuchMidiRoute),

    /// A MIDI call named a target that is not in the graph, or takes no
    /// MIDI (it has no event input): `Midi::play_clip`, `Midi::input`,
    /// `Midi::retarget`, `Midi::record`.
    #[error("{0:?} takes no MIDI: it is not in the graph, or has no event input")]
    NoEventInput(tutti_graph::NodeKey),

    /// A route rule on a session with no MIDI input node (one built
    /// detached).
    #[error("the session has no MIDI input node to route")]
    NoMidiInput,

    /// A hardware call on a session with no OS MIDI layer
    /// ([`SessionBuilder::midi_hardware`](crate::SessionBuilder::midi_hardware)
    /// was not asked for).
    #[cfg(feature = "midi-hardware")]
    #[error("the session has no OS MIDI layer; build it with `midi_hardware()`")]
    NoHardware,

    /// `Midi::retarget` named a node that is not a clip of the session's.
    #[error("{0:?} is not a clip of this session's")]
    NotSessionMidi(tutti_graph::NodeKey),

    /// `Midi::input` found the keys of the target's live input handed out
    /// already: they are the inlet's one producer.
    #[error("the live input of {0:?} has been taken")]
    InputTaken(tutti_graph::NodeKey),

    /// `Midi::input` was asked for the live input of an instrument made by
    /// `Session::add_instrument` (or `Session::instrument`): it is played
    /// through its [`InstrumentHandle`](crate::InstrumentHandle)'s keys.
    #[error("{0:?} is an instrument: play it through its handle's keys")]
    InstrumentKeys(tutti_graph::NodeKey),

    /// `Midi::stop_clip` named a target with no clip.
    #[error("{0:?} has no clip")]
    NoClip(tutti_graph::NodeKey),

    /// A beat that is not finite (NaN or infinite) names no frame: a clip's
    /// start, or an event moved by it.
    #[error("a clip beat must be finite, not {0}")]
    BeatNotFinite(tutti_types::Beat),

    /// `Midi::disconnect_input` named a device that is not an open input.
    #[cfg(feature = "midi-hardware")]
    #[error("no open MIDI input answers to {0:?}")]
    InputNotOpen(tutti_midi_hardware::DeviceQuery),

    /// `Midi::disconnect_output` with no output open.
    #[cfg(feature = "midi-hardware")]
    #[error("no MIDI output is open")]
    OutputNotOpen,

    /// `Midi::retarget_input` named a target with no live input.
    #[error("{0:?} has no live input")]
    NoLiveInput(tutti_graph::NodeKey),

    /// `Midi::follow_tempo` named a Group the MIDI input does not hear
    /// ([`SessionBuilder::midi_groups`](crate::SessionBuilder::midi_groups)).
    #[error("the MIDI input does not hear Group {}", .0.get())]
    GroupNotHeard(tutti_ump::MidiGroup),

    /// `Midi::unfollow_tempo` with no tempo being followed.
    #[error("the session is not following a tempo")]
    NotFollowing,

    /// `Midi::unfollow_clock` with no clock being followed.
    #[error("the session is not following a MIDI clock")]
    NotFollowingClock,

    /// `Midi::unfollow_machine_control` with no Machine Control being followed.
    #[error("the session is not following MIDI Machine Control")]
    NotFollowingMachineControl,

    /// `Midi::stop_sending_machine_control` with no Machine Control being sent.
    #[error("the session is not sending MIDI Machine Control")]
    NotSendingMachineControl,

    /// A tempo has one source: `Midi::follow_tempo` and a Beat Clock
    /// `Midi::follow_clock` each publish the tempo map, so one is refused while
    /// the other is followed.
    #[error("another follower already publishes the tempo; unfollow it first")]
    TempoSourceTaken,

    /// The hardware input's routes have one declarer: the session's own
    /// (`Midi::route`, `Midi::set_route_fallback`) or a host's
    /// (`Midi::set_routes`), not both at once.
    #[error(
        "the MIDI routes are declared by {0}; clear those before declaring \
         them the other way"
    )]
    TwoRouteDeclarers(RouteDeclarer),
}

/// Who holds the hardware input's routes when the other declarer is refused
/// ([`MidiError::TwoRouteDeclarers`]).
#[cfg(feature = "midi")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RouteDeclarer {
    /// The session's own rules: `Midi::route` and `Midi::set_route_fallback`.
    Session,
    /// A host's whole table: `Midi::set_routes`.
    Host,
}

#[cfg(feature = "midi")]
impl std::fmt::Display for RouteDeclarer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Session => "the session's own `route`",
            Self::Host => "a host's `set_routes`",
        })
    }
}

#[cfg(feature = "midi")]
macro_rules! midi_cause_into_error {
    ($($(#[$m:meta])* $ty:ty),* $(,)?) => {$(
        $(#[$m])*
        impl From<$ty> for Error {
            fn from(e: $ty) -> Self {
                Self::Midi(e.into())
            }
        }
    )*};
}

// `?` on a cause reaches the session's one `Error` in a step.
#[cfg(feature = "midi")]
midi_cause_into_error!(
    #[cfg(feature = "midi-hardware")]
    tutti_midi_hardware::Error,
    tutti_midi_file::Error,
    tutti_graph::Refused,
    crate::NoSuchMidiRoute,
);

/// Every subsystem's error, so `?` propagates across crate boundaries.
///
/// Each variant wraps a subsystem error via `#[from]`. Device failures arrive
/// as one `Device` variant (feature `device`) rather than a per-CPAL-call
/// spread: `tutti_cpal::Error` owns that distinction.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
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

    /// An inlet refused its producer's ring ([`Graph::add_inlet`](crate::Graph::add_inlet)).
    #[error(transparent)]
    Inlet(#[from] InletError),

    /// A swap of one node for another was refused
    /// ([`Graph::crossfade`](crate::Graph::crossfade)).
    #[error(transparent)]
    Crossfade(#[from] crate::CrossfadeRefused),

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

    /// A MIDI failure of any kind: the OS port layer, a file, a send, a
    /// route, or a target that takes no MIDI. One variant for the area
    /// (design 030, convention 2); [`MidiError`] says which, each with its
    /// own error as the source.
    #[cfg(feature = "midi")]
    #[error(transparent)]
    Midi(#[from] MidiError),

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

    /// A sampler call or open did not go through ([`SamplerError`](crate::SamplerError)).
    #[cfg(feature = "sampler")]
    #[error(transparent)]
    Sampler(#[from] crate::SamplerError),

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

    /// A dry/wet mix was refused ([`Graph::set_mix`](crate::Graph::set_mix)).
    #[error(transparent)]
    Mix(#[from] tutti_graph::MixError),

    /// The transport's command queue was full ([`Session::play`](crate::Session::play)
    /// and the other verbs).
    #[error(transparent)]
    Transport(#[from] tutti_transport::QueueFull),

    /// A timed transport command was refused
    /// ([`Session::schedule`](crate::Session::schedule)): the list was full, or
    /// the command cannot fire.
    #[error(transparent)]
    Schedule(#[from] ScheduleError),

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

    /// A take could not be armed or did not end cleanly
    /// ([`Session::record`](crate::Session::record)).
    #[cfg(all(feature = "device", feature = "audio-io"))]
    #[error(transparent)]
    Take(#[from] crate::TakeError),

    /// Standard I/O error encountered while reading or writing audio data.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// `Result<T, `[`enum@Error`]`>`.
pub type Result<T> = core::result::Result<T, Error>;
