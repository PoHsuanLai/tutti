//! Transport-driven automation.
//!
//! Trajectory primitives come from the `tutti_curve` crate, re-exported here
//! so consumers need only one import path.
//!
//! Two pieces, one domain — the two directions of a [`Curve`]:
//! - the playback-side [`AutomationLaneNode`] graph node, reading a curve at
//!   the transport's beat position (`beat -> value`) as audio, and the
//!   [`ControlLaneNode`], which sends a target's whole `ControlProgram`
//!   (automation and modulation) as curve-segment events for a param port
//!   (a piece's [`Law`] rides in the segment);
//! - the capture-side [`Recorder`], fed `(beat, value)` samples during a write /
//!   touch / latch take and handing back an trajectory.
//!
//! The map from a host's target vocabulary to its recorders stays host-side; the
//! engine holds no registry and no target trait. See [`Recorder`] for why.
//!
//! The Bevy ECS binding (lane-node spawn, param reconcile) is the host's: it
//! writes `Volume`/`Pan`/`PluginParam` DAW components, which are not the
//! engine's vocabulary.

mod control_lane;
mod lane;
mod recording;

// The `Curve` trait (beat → value) lives in tutti-modulation — modulation depends on it
// and tutti-nodes already depends on tutti-modulation, so the trait is homed there and
// re-exported here for the automation consumers.
#[doc(hidden)]
pub use control_lane::LaneProbe;
pub use control_lane::{CONTROL_STEP, ControlLaneNode, LANE_EVENT_CAPACITY, LaneControls};
pub use lane::{AutomationLaneNode, LiveAutomationLane};
pub use recording::{RecordMode, Recorder, RecordingConfig};
pub use tutti_modulation::Curve;

pub use tutti_curve::{Breakpoint, Law, Trajectory};
