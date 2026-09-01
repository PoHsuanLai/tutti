//! Transport-driven envelope automation.
//!
//! Envelope primitives come from the `audio_automation` crate, re-exported here
//! so consumers need only one import path.
//!
//! Two pieces, one domain — the two directions of a [`Curve`]:
//! - the playback-side [`AutomationLaneNode`] `AudioUnit`, reading a curve at the
//!   transport's beat position (`beat -> value`);
//! - the capture-side [`Recorder`], fed `(beat, value)` samples during a write /
//!   touch / latch take and handing back an envelope.
//!
//! The map from a host's target vocabulary to its recorders stays host-side; the
//! engine holds no registry and no target trait. See [`Recorder`] for why.
//!
//! The Bevy ECS binding (lane-node spawn, param reconcile) is app-side, in
//! `dawai_model::audio_graph`: it writes the `Volume`/`Pan`/`PluginParam` DAW
//! components, which are not the engine's vocabulary.

mod lane;
mod recording;

// The `Curve` trait (beat → value) lives in tutti-mod — modulation depends on it
// and tutti-nodes already depends on tutti-mod, so the trait is homed there and
// re-exported here for the automation consumers.
pub use lane::{AutomationLaneNode, LiveAutomationLane};
pub use recording::{RecordMode, Recorder, RecordingConfig};
pub use tutti_mod::Curve;

pub use audio_automation::{
    AutomationClip, AutomationEnvelope, AutomationPoint, AutomationState, CurveType,
};
