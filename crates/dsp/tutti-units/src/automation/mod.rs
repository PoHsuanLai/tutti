//! Transport-driven envelope automation.
//!
//! Was previously its own `tutti-automation` crate; folded in once the
//! only-uses-it consumer (`AutomationLane` as an `AudioUnit`) made the
//! extra workspace member pointless. Envelope primitives still come from
//! the `audio_automation` crate — re-exported here so consumers only need
//! one import path.
//!
//! Two pieces, one domain — the two directions of a [`Curve`]:
//! - [`lane`] — the playback-side [`AutomationLane`] `AudioUnit`, reading a
//!   curve at the transport's beat position (`beat -> value`).
//! - [`recording`] — the capture-side [`Recorder`], fed `(beat, value)` samples
//!   during a write / touch / latch take and handing back an envelope.
//!
//! The map from a host's target vocabulary to its recorders stays host-side; the
//! engine holds no registry and no target trait. See the [`recording`] module
//! docs for why.
//!
//! The Bevy ECS binding (lane-node spawn + param reconcile +
//! `TuttiAutomationPlugin`) moved app-side to
//! `dawai_model::engine_bind::automation` — it wrote the `Volume`/`Pan`/
//! `PluginParam` DAW components, which left the engine.

mod lane;
mod recording;

// The `Curve` trait (beat → value) lives in tutti-mod — modulation depends on it
// and tutti-units already depends on tutti-mod, so the trait is homed there and
// re-exported here for the automation consumers.
pub use lane::{AutomationLane, LiveAutomationLane};
pub use recording::{RecordMode, Recorder, RecordingConfig};
pub use tutti_mod::Curve;

pub use audio_automation::{
    AutomationClip, AutomationEnvelope, AutomationPoint, AutomationState, CurveType,
};
