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
//! commands.spawn(ExportRequest {
//!     source: ExportSource::Master,
//!     target: ExportTarget::File {
//!         path: "mix.wav".into(),
//!         normalize: Some(Normalize::lufs(Db(-14.0))),
//!     },
//!     config,
//!     clock: Arc::new(FrozenClock),
//! })
//! .observe(|done: On<ExportDone>| { /* ... */ });
//!
//! // One node's output, into memory.
//! commands.spawn(ExportRequest {
//!     source: ExportSource::Node(node_id),
//!     target: ExportTarget::Buffers,
//!     config,
//!     clock,
//! });
//! ```
//!
//! # What is deliberately not here
//!
//! **No queue.** No `max_in_flight`, no priority field, no slot accounting. A
//! cap in this crate would make unrelated consumers queue behind one another —
//! a long project export starving a view's taps — and would force two crates
//! that never meet to agree on an integer convention. What the engine gives a
//! caller instead is the fact it needs to throttle itself: [`ExportInFlight`]
//! is a public component, so "one at a time" is
//! `run_if(not(any_with_component::<ExportInFlight>))` at the caller's own
//! spawn site, where it also knows which of *its* requests matters most.
//!
//! **No progress reporting.** The render is one synchronous call; there is
//! nothing to sample between blocks without the engine pushing a callback back
//! across the task boundary.

mod request;
mod run;

pub use request::{
    ExportDone, ExportInFlight, ExportOutput, ExportRequest, ExportSource, ExportTarget,
    NetPopulator, PopulateNet,
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
        // Chained the other way, a render that finishes quickly is started and
        // reported within one frame — `ExportInFlight` is inserted and removed
        // before any other system observes it. That silently breaks the one
        // throttling mechanism this crate offers: a caller gating on
        // `not(any_with_component::<ExportInFlight>)` would never see a short
        // render, and would spawn a second one on top of it.
        //
        // Polling first means a render is always visible for at least one full
        // frame, and costs only that a just-finished render reports on the next
        // frame rather than the current one.
        app.add_systems(
            Update,
            (poll_exports, start_exports.run_if(crate::graph::engine_ready)).chain(),
        );
    }
}
