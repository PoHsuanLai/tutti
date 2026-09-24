//! Offline export, driven from the ECS.
//!
//! `tutti-export` is synchronous and Bevy-free on purpose — it spawns no
//! threads, because "a host that wants a render off the main thread already
//! owns a task pool that is better at it than a raw `std::thread` would be".
//! This module is that host: it carries the engine's own `ExportConfig` as a
//! component, runs the synchronous call on Bevy's `AsyncComputeTaskPool`, and
//! reports the result as an entity event.
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
//! use tutti_core::dsp::Net;
//! use tutti_nodes::testing::Const;
//! use tutti_export::{
//!     AudioFormat, BitDepth, ChannelLayout, EncodeConfig, ExportConfig, FrozenClock,
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
//! let mut net = Net::with_backend(2);
//! net.master(Const::mono(0.5));
//!
//! let mut app = App::new();
//! app.add_plugins((bevy_app::TaskPoolPlugin::default(), ExportPlugin));
//! app.insert_resource(AudioGraphRes(net));
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
//!         Arc::new(FrozenClock),
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
//! A request may also carry a `with_prepare` hook — the last look at the net
//! before it leaves the main thread, which is where an isolated clone's voices
//! get refilled from the app's world. See [`ExportRequest::with_prepare`].
//!
//! # What is deliberately not here
//!
//! **No priority.** Requests start oldest-first, one per frame. There is no
//! priority field, because a cross-crate integer convention would make two
//! consumers that never meet argue about whose renders matter — and the caller
//! that knows the answer can simply spawn the one it wants first.
//!
//! There *is* a cap, of exactly one: see [`ExportInFlight`]. It is enforced in
//! [`start_exports`] rather than left to callers, because the per-request net
//! clone is main-thread work and a `run_if` gate cannot see requests spawned in
//! its own frame.
//!
//! **No progress reporting.** The render is one synchronous call; there is
//! nothing to sample between blocks without the engine pushing a callback back
//! across the task boundary. Cancellation *is* available —
//! [`ExportInFlight::cancel`], or just despawn the entity.

mod request;
mod run;

pub use request::{
    ExportDone, ExportInFlight, ExportOutput, ExportRequest, ExportSource, ExportTarget,
    PrepareNet, PreparedNet,
};
pub use run::{poll_exports, start_exports};

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;

/// Runs [`ExportRequest`]s on the compute pool.
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
    }
}
