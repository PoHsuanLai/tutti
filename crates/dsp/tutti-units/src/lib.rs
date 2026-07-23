//! DSP nodes for the Tutti audio engine.

// The crate's only fallible operation is VBAP speaker-layout construction, so
// `Error` / `Result` exist only under `spatial` (without it `Error` would be an
// uninhabited enum with no users).
#[cfg(feature = "spatial")]
mod error;
#[cfg(feature = "spatial")]
pub use error::{Error, Result};

mod node_id;

pub use tutti_core::{
    params, Bpm, Cents, Db, Degrees, Hz, Linear, Param, Ratio, SampleRate, Seconds, Semitones, Unit,
};

// NOTE: the shared DSP param pool (`Frequency`/`FilterQ`/`WetMix`/…), the node
// authoring markers (`FilterNode`/`ReverbNode`/…), the generic marker spawner
// (`spawn_dsp_node`/`TuttiDspPlugin`), the param reconcilers
// (`reconcile_unit_params`/`reconcile_reverb_params`/`reconcile_convolver_params`),
// and the deferred convolver load moved OUT of this crate to
// `dawai_model::engine_bind` — they are DAW-param ECS policy, not DSP. This
// crate keeps only the pure DSP unit types + the `set(UnitParam)` surface the
// app drives them through. (Engine Bevy = Net pump only.)

pub mod buffer;

mod lfo;
pub use lfo::{LfoMode, LfoNode, LfoShape};

mod delay;
pub use delay::{DelayLine, DelayLineNode, InterpolationMode, StereoDelayLineNode, StereoPair};

mod distortion;
pub use distortion::{DistortionNode, ShapeKind};

mod filter;
pub use filter::{
    BandState, EqBandNode, LadderFilterNode, LadderType, StereoLadderFilterNode,
    StereoSvfFilterNode, SvfFilterNode, SvfType,
};

mod dynamics;
pub use dynamics::{BrickwallLimiter, Compressor, Gate, LimiterNode};

// A node declares its own audio-rate param-input ports (cutoff, drive, …). No
// Bevy dependency — pure node capability.
mod param_ports;
pub use param_ports::ParamPorts;

#[cfg(feature = "spatial")]
mod spatial;
#[cfg(feature = "spatial")]
pub use spatial::{BinauralPannerNode, ChannelLayout, SpatialPannerNode};

mod modulation;
pub use modulation::{ChorusNode, FlangerNode, PhaserNode, StereoPhaserNode};

#[cfg(feature = "convolution")]
mod convolution;
#[cfg(feature = "convolution")]
pub use convolution::{
    generate_room_ir, generate_room_ir_into, generate_test_ir, generate_test_ir_into, Convolver,
    ConvolverNode, IrChannelConfig, StereoConvolverNode, WetDry,
};

/// Transport-driven envelope automation — the playback-side `AutomationLane`
/// `AudioUnit`, the recording-side `Manager`/`Recorder`, and the Bevy ECS
/// binding ([`automation::graph`]). See the module docs.
#[cfg(feature = "automation")]
pub mod automation;

// Bevy ECS integration for the units domain that STILL lives here — the spatial
// panner + automation graph bindings (they bind onto tutti-core's foundational
// `Volume`/`Pan`/`PluginParam` params, which have not moved out yet). The DSP
// param/marker/spawn/reconcile cluster moved to `dawai_model::engine_bind`.
//
// The `spatial_graph` suffix disambiguates the graph-binding layer from the
// same-named `spatial/` DSP-unit module it drives (the panner nodes). The
// automation graph binding lives inside `automation::graph` alongside its DSP.
#[cfg(all(feature = "bevy", feature = "spatial"))]
pub mod spatial_graph;
#[cfg(all(feature = "bevy", feature = "spatial"))]
pub use spatial_graph::{
    spatial_audio_sync_system, AttenuationModel, AudioListener, SpatialAudio, TuttiSpatialPlugin,
};

#[cfg(all(feature = "bevy", feature = "automation"))]
pub use automation::graph::{
    automation_lane_system, reconcile_automation_writes, update_automation_envelope_system,
    AddAutomationLane, AutomationDrivesParam, AutomationLaneEmitter, AutomationLaneNode,
    AutomationParam, TuttiAutomationPlugin, UpdateAutomationEnvelope,
};
