//! Offline export, driven from the ECS (feature `export`).
//!
//! The session exports (`tutti_session::Session::export`: a job that is
//! `Send` and blocks, and `finish_export` on the main thread). This module
//! runs it for a Bevy host: it carries the engine's own `ExportConfig` as a
//! component, runs the job on Bevy's `AsyncComputeTaskPool`, and reports the
//! result as an entity event.
//!
//! An export is an **entity**: spawn a request, observe the result on that same
//! entity, where the code that knows what the render was for still has scope.
//!
//! ```rust
//! use std::sync::{Arc, Mutex};
//!
//! use bevy_app::prelude::*;
//! use bevy_ecs::prelude::*;
//! use bevy_tutti::prelude::*;
//! use bevy_tutti::graph::AudioConfig;
//! use tutti_nodes::testing::Const;
//! use tutti_export::{
//!     AudioFormat, BitDepth, ChannelLayout, EncodeConfig, ExportConfig,
//!     RenderConfig,
//! };
//!
//! let config = ExportConfig {
//!     render: RenderConfig {
//!         sample_rate: tutti_core::SampleRate(44_100.0),
//!         duration_seconds: 0.05,
//!         ..Default::default()
//!     },
//!     encode: EncodeConfig {
//!         format: AudioFormat::Wav,
//!         bit_depth: BitDepth::Float32,
//!         channels: ChannelLayout::STEREO,
//!     },
//!     ..Default::default()
//! };
//!
//! let mut graph = AudioGraphRes::headless(0, 2);
//! let (node, _) = graph.insert(Const::mono(0.5));
//! graph.set_outputs_from(node);
//!
//! let mut app = App::new();
//! app.add_plugins((bevy_app::TaskPoolPlugin::default(), ExportPlugin));
//! app.insert_resource(graph);
//! app.insert_resource(AudioConfig {
//!     sample_rate: tutti_core::SampleRate(44_100.0),
//!     channels: ChannelLayout::STEREO,
//! });
//! // `start_exports` is gated on the engine state, not on the graph resource.
//! app.insert_resource(AudioEngineState::Running);
//!
//! // The whole graph into memory. `ExportTarget::File { path, normalize }`
//! // encodes to disk instead — `normalize: None` streams, `Some(..)` is a
//! // two-pass render that holds the signal to measure a gain from it.
//! let channels: Arc<Mutex<Option<usize>>> = Arc::default();
//! let seen = channels.clone();
//! app.world_mut()
//!     .spawn(ExportRequest::new(
//!         ExportSource::Master,
//!         ExportTarget::Buffers,
//!         config,
//!         ExportClock::frozen(),
//!     ))
//!     .observe(move |done: On<ExportDone>| {
//!         if let Ok(ExportOutput::Buffers(rendered)) = &done.result {
//!             *seen.lock().unwrap() = Some(rendered.channels());
//!         }
//!     });
//!
//! // The render runs on the task pool, so the app has to keep ticking.
//! for _ in 0..2000 {
//!     app.update();
//!     if channels.lock().unwrap().is_some() {
//!         break;
//!     }
//!     std::thread::sleep(std::time::Duration::from_millis(2));
//! }
//!
//! assert_eq!(*channels.lock().unwrap(), Some(2));
//! ```
//!
//! A request may also carry a `with_prepare` hook — a look at the app's
//! world in the frame the export starts, returning an edit of the graph
//! made once the fork is built, which is where an isolated copy's voices get
//! refilled from the app's world. See [`ExportRequest::with_prepare`].
//!
//! # Which graph is rendered
//!
//! An export renders a **fork** of the live graph (`Editor::fork`): what the
//! global outputs hear for [`ExportSource::Master`], or exactly the sub-graph
//! feeding one node for [`ExportSource::Node`], every node isolated, rebound
//! onto the request's [`ExportClock`] and reset. The live graph is not
//! touched and keeps playing while the render runs on the pool.
//!
//! - **It renders what the graph is driven to play, from silence.** A fork
//!   keeps neither the live transport bindings nor running state (delay
//!   lines, a sounding voice).
//! - **Controls are a snapshot** at the fork: a parameter moved while the
//!   render runs does not reach it. A modulated parameter renders the
//!   same modulation as live: its control lane is a graph node, forked
//!   with the graph, and sends its value on the fork's first block; a
//!   hosted plugin renders its saved state plus the lanes on its event
//!   input.
//! - **A hosted plugin is forked by state transfer**: a fresh instance in a
//!   new `plugin-server` process, loaded with the live one's state, told it
//!   is rendering offline. Its MIDI clip comes with it, rebound onto the
//!   render's timeline, so an exported instrument plays its notes; its live
//!   MIDI inbox does not. Its state is taken in the frame the export
//!   starts; the new server is launched and loaded on the compute pool, so
//!   that frame does not wait for it.
//! - **A built-in synth plays its clip too**: a `PolySynth` or
//!   `SoundFontUnit` forks through its own source, which rebinds the clip
//!   installed on the live synth's port onto the render's timeline (a
//!   SoundFont fork also renders at the export's rate). A MIDI source that
//!   cannot be rebound refuses the export by name
//!   ([`ExportError::ForkSource`]) rather than render its notes as silence,
//!   for a synth as for a plugin.
//! - **A disk-streamed sampler voice reads its file itself.** Its copy
//!   cannot play through the butler (the live audio thread is the ring's one
//!   consumer, and a seek moves the live stream), so it decodes the file the
//!   stream plays on the render's thread, and plays the voice's window of it
//!   on the request's timeline, resampled to the render's rate and looped as
//!   the stream is looped when the export starts. The live voice and its
//!   butler are not touched.
//! - **Some nodes cannot be forked**, and an export that needs one (an
//!   output reaches it) is refused naming the node's entity
//!   ([`ExportError::NotForkable`]): a microphone monitor. Export a node it
//!   does not feed. A hosted plugin forks by state transfer — out of process
//!   into a new `plugin-server`, an in-process VST2 plugin into a second
//!   instance from the same library — and one whose fork cannot be built
//!   (it will not save its state, or a second instance will not load) is
//!   refused naming it too ([`ExportError::ForkSource`]).
//! - **A fork that fails while rendering** — a plugin server that crashes or
//!   hangs, a disk voice whose file cannot be read (its cause names the
//!   file) — fails the export by name ([`ExportError::ForkFailed`]) rather
//!   than writing silence as a success.
//!
//! # What is deliberately not here
//!
//! **No priority.** Requests start oldest-first, one per frame; spawn the one
//! you want first, first.
//!
//! There *is* a cap, of exactly one: see [`ExportInFlight`]. It is enforced in
//! [`start_exports`] rather than left to callers, because the per-request
//! gather of the graph copy is main-thread work and a `run_if` gate cannot see
//! requests spawned in its own frame.
//!
//! **No progress reporting.** The render is one synchronous call; there is
//! nothing to sample between blocks without the engine pushing a callback back
//! across the task boundary. Cancellation *is* available —
//! [`ExportInFlight::cancel`], or just despawn the entity.

mod request;
mod run;

pub use request::{
    ExportClock, ExportDone, ExportError, ExportInFlight, ExportNode, ExportOutput, ExportRequest,
    ExportSource, ExportTarget, PrepareEdit, PrepareGraph, PreparedGraph,
};

pub use run::{poll_exports, start_exports};
// The engine types an `ExportError` carries, so a host names them without
// depending on the crates they come from. Not the graph a `prepare` hook
// edits: `PreparedGraph::graph` is `tutti_export::RenderGraph`, the session's
// offline render, which this adapter does not name (design 015 §8, check 3,
// `scripts/check-adapter.sh`). A hook calls its methods without naming it;
// a host that names it depends on `tutti-export`.
pub use tutti_graph::{ForkCause, ForkFaultKind};
pub use tutti_types::NodeKey;

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;

/// Runs [`ExportRequest`]s on the compute pool.
///
/// Added by [`TuttiPlugin`](crate::TuttiPlugin) with the `export` feature.
/// Needs Bevy's task pools (`bevy_app::TaskPoolPlugin`, part of
/// `DefaultPlugins`).
///
/// `start_exports` is gated on the engine being ready (it reads the graph);
/// `poll_exports` is not, so a render still in flight when the engine tears
/// down is still reported rather than left hanging.
pub struct ExportPlugin;

impl Plugin for ExportPlugin {
    fn build(&self, app: &mut App) {
        // Poll BEFORE start, deliberately.
        //
        // Chained the other way, a render that finishes inside one frame is
        // started and reported before any other system sees `ExportInFlight` —
        // so anything watching that component to know whether an export is
        // running would miss short renders entirely.
        //
        // Polling first also means `start_exports` observes the previous
        // render's completion in the same frame it picks the next request, so
        // a queue of them drains at one per frame with no idle gap. The cost is
        // only that a just-finished render reports on the following frame.
        app.add_systems(
            Update,
            (
                poll_exports,
                start_exports.run_if(crate::graph::engine_ready),
            )
                .chain(),
        );

        // The plugins' render mode (the session's `Phase::RenderMode`),
        // after both: `poll_exports` finishes without announcing, so this is
        // what tells the plugins `Realtime` once the last export of a queue
        // ends, and the frame after a cancelled one. Scheduled here, not by
        // `TuttiHostingPlugin`: a frame that finishes an export must run it,
        // whichever plugin was added first. Ungated on the engine, as
        // `poll_exports` is: a render in flight when audio stops still has
        // to put its plugins back.
        //
        // Its set runs after `GraphReconcileSystems::Spawn` and before
        // `Params` (`configure_phases`), so with `plugin` this also forces
        // `start_exports`, which forks the live graph, before the frame's
        // `Params` and `Commit`. Before design 015 PR 7 the two were
        // unordered. Without `plugin`, they still are.
        #[cfg(feature = "plugin")]
        app.add_systems(
            Update,
            crate::plugin_host::plugin_render_mode
                .after(poll_exports)
                .after(start_exports)
                .in_set(crate::graph::SessionPhaseSystems(
                    tutti_session::Phase::RenderMode,
                )),
        );
    }
}
