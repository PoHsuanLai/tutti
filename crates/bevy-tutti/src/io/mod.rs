//! The live audio I/O edge: mic in, WAV out, and the pump between them.
//!
//! One module per engine crate is this adapter's shape, and the engine's live
//! edge is [`tutti_io`]. What that crate defines — [`MicMonitorNode`],
//! [`WavOut`], [`Recorder`] — is surfaced here rather than through
//! [`engine`](crate::engine), which is bootstrap and owns none of it. The
//! device half ([`MicIn`]) is `tutti-cpal`'s and comes through with them, so a
//! host reaches the whole capture path from one import.
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
//! to a pump, and [`MicMonitorNode`] is an `AudioUnit` declared through
//! [`AudioSources`](crate::graph::AudioSources) like any other node.
//!
//! # [`Recorder`] or [`AudioPump`](crate::graph::AudioPump)?
//!
//! Same loop; they differ in who owns the stopping. **In a Bevy app, use
//! `AudioPump`** — it is a `Component`, so despawning finalizes the sink and
//! `PumpFinished` carries the result. A forgotten `Recorder` in a resource
//! leaves a WAV whose header was never patched.
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
//! ```rust,ignore
//! let (mic, monitor) = MicIn::open_with_monitor(None)?;
//! let id = graph.0.add(monitor);
//! let node = commands.spawn(AudioNode(id)).id();
//! commands.insert_resource(MasterSources::from(node));
//! ```
//!
//! That this composes at all — the monitor reconciling like any other node,
//! reaching an effect chain, and carrying audio once declared — is pinned by
//! `tests/io_graph_composition.rs`, including the undeclared-monitor silence
//! described above.
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
//! ```rust,ignore
//! let mic = MicIn::open(None)?;
//! let wav = mic.matching_sink(&path, BitDepth::Float32).expect("sink opens");
//! commands.spawn(AudioPump::start(mic, wav, 1024));
//! ```
//!
//! For any other source the obligation stays the caller's;
//! [`WavOut::sample_rate`] reports what the sink promised so it can be compared.
//!
//! # Recording what the graph is playing
//!
//! The mic is one source; the *master output* is the other one hosts ask for.
//! [`AudioTapRes`](crate::graph::AudioTapRes) is the engine's lock-free copy of
//! that output, and [`TapIn`] adapts its consumer end into an [`AudioIn`] so the
//! same pump records either:
//!
//! ```rust,ignore
//! let src = TapIn::new(tap.open().expect("tap is free"));
//! let wav = WavOut::create(&path, config.sample_rate, ChannelLayout::STEREO, BitDepth::Float32)?;
//! commands.spawn(AudioPump::start(src, wav, 1024));
//! ```
//!
//! Two things this cannot do for you. The tap is **opt-in** — until `open()` is
//! called the audio thread pays one atomic load and pushes nothing — and it
//! serves one consumer at a time, so `open()` fails rather than displacing an
//! analysis reader that got there first. And the rate is yours to match: a tap
//! has no device to ask, so it is
//! [`AudioConfig`](crate::graph::AudioConfig) that knows, not this module.

// Re-exported, not wrapped — see the module docs. `MicIn` owns a CPAL input
// stream (the device layer); the other three are device-free and live one crate
// below it, in `tutti-io`.
pub use tutti_cpal::MicIn;
pub use tutti_io::{MicMonitorNode, MicRing, Recorder, TapIn, WavOut};

// The vocabulary those types speak. `BitDepth` selects a sink's on-disk width;
// `OnEmpty` is what a source says a 0-frame poll means, which is why a pump
// takes no policy argument; `ChannelLayout` is the *channel* width, which the
// I/O traits carry at runtime rather than as a const parameter — so a host
// building a source or a sink needs it named here, not fetched from `tutti-core`.
pub use tutti_core::io::{pump, AudioIn, AudioOut, OnEmpty};
pub use tutti_core::pcm::BitDepth;
pub use tutti_core::ChannelLayout;
