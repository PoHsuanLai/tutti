//! Offline export, driven from the ECS.
//!
//! `tutti-export` is synchronous and Bevy-free on purpose — it spawns no
//! threads, because "a host that wants a render off the main thread already
//! owns a task pool that is better at it than a raw `std::thread` would be".
//! This module is that host: it carries the engine's own `ExportConfig` as a
//! component, runs the synchronous call on Bevy's `AsyncComputeTaskPool`, and
//! reports the result as an entity event.
//!
//! ```ignore
//! // Whole graph to a normalized file.
//! commands.spawn(ExportRequest::new(
//!     ExportSource::Master,
//!     ExportTarget::File {
//!         path: "mix.wav".into(),
//!         normalize: Some(Normalize::lufs(Db(-14.0))),
//!     },
//!     config,
//!     Arc::new(FrozenClock),
//! ))
//! .observe(|done: On<ExportDone>| { /* ... */ });
//!
//! // One node's output into memory, filling its voices from the app's world
//! // on the way past.
//! commands.spawn(
//!     ExportRequest::new(ExportSource::Node(node_id), ExportTarget::Buffers, config, clock)
//!         .with_prepare(|prepared, world| fill_voices(prepared.net, prepared.ctx, world)),
//! );
//! ```
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
