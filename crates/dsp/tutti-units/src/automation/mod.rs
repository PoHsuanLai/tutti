//! Transport-driven envelope automation.
//!
//! Was previously its own `tutti-automation` crate; folded in once the
//! only-uses-it consumer (`AutomationLane` as an `AudioUnit`) made the
//! extra workspace member pointless. Envelope primitives still come from
//! the `audio_automation` crate — re-exported here so consumers only need
//! one import path.
//!
//! Three pieces, one domain:
//! - [`lane`] — the playback-side [`AutomationLane`] `AudioUnit` (envelope
//!   value at the transport's beat position).
//! - [`recording`] — the recording-side [`Manager`] / [`Recorder`] /
//!   [`RecordingTarget`] (write / touch / latch capture during a take).
//! - [`graph`] — the Bevy ECS binding: lane-node spawn + param reconcile +
//!   [`TuttiAutomationPlugin`](graph::TuttiAutomationPlugin).

#[cfg(feature = "bevy")]
pub mod graph;
mod lane;
mod recording;

pub use lane::{AutomationLane, LiveAutomationLane};
pub use recording::{
    AutomationRecordingConfig, AutomationSnapshot, AutomationTarget, Manager, Recorder,
    RecordingTarget,
};

pub use audio_automation::{
    AutomationClip, AutomationEnvelope, AutomationPoint, AutomationState, CurveType,
};
