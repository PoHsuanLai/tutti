//! The live audio I/O edge: mic in, WAV out, and the pump between them.
//!
//! Re-exports [`tutti_io`]'s [`MicMonitorNode`], [`WavOut`], [`Recorder`] and
//! [`TapIn`], and `tutti-cpal`'s capture device [`MicIn`], so a host reaches
//! the whole capture path from one import.
//!
//! Gated on `audio-io`, independent of `sampler`: recording a take needs no
//! clip playback, and playing a clip needs no microphone.
//!
//! # No wrapper here, deliberately
//!
//! Every type below is a plain re-export. The one thing needing ECS ownership
//! is the pump's *lifetime* — a background thread whose sink must be finalized
//! exactly once — and that is [`AudioPump`](crate::graph::AudioPump), over in
//! [`graph`](crate::graph) with the rest of the per-frame machinery.
//!
//! The rest is already the right shape to hold directly: [`WavOut`] is handed
//! to a pump, and [`MicMonitorNode`] is a graph node
//! ([`Graph::insert`](crate::graph::Graph::insert),
//! unforkable: an export of a graph holding the live mic is refused, naming
//! it) declared through [`PortSources`](crate::graph::PortSources) like any
//! other node.
//!
//! # [`Recorder`] or [`AudioPump`](crate::graph::AudioPump)?
//!
//! One loop: an `AudioPump` is a `Component` holding a `Recorder`. They differ
//! in who owns the stopping. **In a Bevy app, use `AudioPump`** — despawning
//! finalizes the sink, a finite source's end is noticed by a system without
//! blocking the frame, and `PumpFinished` carries the result (a width mismatch
//! included). A `Recorder` kept in a resource finalizes only when it is
//! stopped or dropped, and nothing reports its result to the world.
//!
//! `Recorder` is for hosts with no ECS: a CLI, a test, a headless render.
//!
//! # Two traps this module cannot remove
//!
//! **An unwired monitor drops frames in silence.** [`MicMonitorNode`] drains a
//! ~10 ms ring the capture callback fills. Add it to the graph but never
//! declare what it feeds and the ring fills, then every later frame is
//! discarded — no error, no counter, and a monitor that looks connected. The
//! fix is to declare it, exactly like any node:
//!
//! ```rust,no_run
//! // `no_run`: `MicIn::open_with_monitor` opens a real capture device.
//! use bevy_ecs::prelude::*;
//! use bevy_tutti::prelude::*;
//!
//! fn monitor_mic(
//!     mut graph: ResMut<AudioGraphRes>,
//!     config: Res<bevy_tutti::graph::AudioConfig>,
//!     mut commands: Commands,
//! ) {
//!     // The GRAPH's rate, not the device's. `MicMonitorNode` does not
//!     // resample, so a mic that cannot run at this rate is an error rather
//!     // than a drift nobody reports.
//!     let (_mic, monitor) = MicIn::open_with_monitor(None, config.sample_rate)
//!         .expect("a capture device at the graph rate");
//!     let (id, ()) = graph.graph().insert(monitor);
//!     let node = commands.spawn(AudioNode(id)).id();
//!     // Without this the ring fills and every later frame is discarded, with
//!     // no error and no counter.
//!     commands.insert_resource(MasterSources::from(node));
//! }
//! ```
//!
//! **A source and sink that disagree produce a wrong-speed file.**
//! [`Recorder`] and [`AudioPump`](crate::graph::AudioPump) both take an already
//! built sink, and nothing between them can check the pairing — `AudioIn`
//! deliberately carries no sample rate. Feed 48 kHz frames to a sink that
//! declared 8 kHz and the result is a valid WAV that plays back six times too
//! slow, silently.
//!
//! For a mic, use [`MicIn::matching_sink`], which pairs them at the one place
//! both halves are in scope:
//!
//! ```rust,no_run
//! // `no_run`: `MicIn::open` opens a real capture device.
//! use bevy_ecs::prelude::*;
//! use bevy_tutti::prelude::*;
//!
//! fn record_mic(path: std::path::PathBuf, rate: SampleRate, mut commands: Commands) {
//!     let mic = MicIn::open(None, rate).expect("a capture device at the graph rate");
//!     // Paired at the one place both halves are in scope, so the sink cannot
//!     // declare a rate the source does not produce.
//!     let wav = mic.matching_sink(path, Pcm::Float32).expect("sink opens");
//!     commands.spawn(AudioPump::start(mic, wav, Samples(1024)));
//! }
//! ```
//!
//! For any other source the obligation stays the caller's;
//! [`WavOut::sample_rate`] reports what the sink promised so it can be compared.
//!
//! # Recording what the graph is playing
//!
//! The mic is one source; what the output sounds like is the other one hosts
//! ask for. [`AudioGraphRes::open_heard_tap`](crate::graph::AudioGraphRes::open_heard_tap)
//! is the tap on it: the engine's output at the device's width, after the
//! declick fader, so a take across a transport stop fades as the device does.
//! [`TapIn`] adapts the [`TapReader`](tutti_graph::TapReader) it returns into an
//! [`AudioIn`] so the same pump records either:
//!
//! ```rust,no_run
//! use bevy_app::prelude::*;
//! use bevy_ecs::prelude::*;
//! use bevy_tutti::graph::AudioConfig;
//! use bevy_tutti::prelude::*;
//!
//! /// Where the take lands. A real host reads this off its project settings.
//! #[derive(Resource)]
//! struct TakePath(std::path::PathBuf);
//!
//! fn record_master(
//!     graph: Res<AudioGraphRes>,
//!     config: Res<AudioConfig>,
//!     path: Res<TakePath>,
//!     mut commands: Commands,
//! ) {
//!     // Three seconds of ring: what the pump does not drain in time is
//!     // counted (`TapReader::take_lost`), not blocked on.
//!     let reader = graph.open_heard_tap(Samples(144_000)).expect("an engine, and no other reader");
//!     // The sink is as wide as the tap (the device); the rate is `AudioConfig`'s.
//!     let wav = WavOut::create(&path.0, config.sample_rate, reader.layout(), Pcm::Float32)
//!         .expect("a writable path");
//!     commands.spawn(AudioPump::start(TapIn::new(reader), wav, Samples(1024)));
//! }
//! # let mut app = App::new();
//! # app.add_systems(Startup, record_master);
//! ```
//!
//! (`no_run`: the heard tap needs the engine a device session builds.) Things
//! to know:
//!
//! - The heard tap flows from the first render that begins after it opens,
//!   needs no commit, and is fed on every path that renders through the engine
//!   (the device callback, an offline `render`), silence included when a block
//!   is refused. An export renders a fork without the engine and never writes
//!   to it. It keeps **every device channel** (no stereo fold), one reader at a
//!   time.
//! - [`Graph::open_output_tap`](tutti_session::Graph::open_output_tap) (through
//!   [`AudioGraphRes::graph`](crate::graph::AudioGraphRes::graph)) is the
//!   explicit choice of the graph's own signal, aligned by the compiler, on any
//!   ports and several at once, but *before* the declick fader: a
//!   transport-gated source (a clip, the click) steps from full level to
//!   silence at a stop where the device output fades, so a take across a stop
//!   has a click there (a source that keeps running has none).
//! - A pump that falls behind loses what the ring could not hold and the file
//!   closes up over the gap;
//!   [`PumpFinished::lost`](crate::graph::PumpFinished::lost) says how many
//!   frames.
//! - The rate is yours to match: a tap has no device to ask, so it is
//!   [`AudioConfig`](crate::graph::AudioConfig) that knows, not this module.

// Re-exported, not wrapped — see the module docs. `MicIn` owns a CPAL input
// stream (the device layer); the other three are device-free and live one crate
// below it, in `tutti-io`.
pub use tutti_cpal::MicIn;
pub use tutti_io::{MicMonitorNode, MicRing, Recorder, TapIn, WavOut};

// The vocabulary those types speak. `Pcm` selects a sink's on-disk width;
// `OnEmpty` is what a source says a 0-frame poll means, which is why a pump
// takes no policy argument; `ChannelLayout` is the *channel* width, which the
// I/O traits carry at runtime rather than as a const parameter — so a host
// building a source or a sink needs it named here, not fetched from `tutti-metering`.
// `Samples` is the frame count `poll_into` and `pump` return, and the unit an
// `AudioPump`'s capacity is given in — the same "needs it named here" reason.
pub use tutti_codec::Pcm;
pub use tutti_types::{AudioIn, AudioOut, OnEmpty, pump};
pub use tutti_types::{ChannelLayout, Samples};
