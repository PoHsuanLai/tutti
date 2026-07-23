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

// The shared DSP parameter pool (Frequency, FilterQ, GainDb, WetMix, …) and the
// node authoring markers — the cross-cutting ECS vocabulary the reconcile/spawn
// systems below read. Moved out of tutti-core (which keeps only the foundational
// graph params) so the API sits in the crate whose DSP it drives. Bevy-only.
#[cfg(feature = "bevy")]
pub mod dsp_params;
#[cfg(feature = "bevy")]
pub mod node_markers;
#[cfg(feature = "bevy")]
pub use dsp_params::{
    Attack, Azimuth, BeatSynced, CeilingDb, CompressorRatio, DelayTime, Drive, Elevation, Feedback,
    FilterMode, FilterQ, Frequency, GainDb, LfoShapeKind, MaxDelay, ModDepth, ModRate, Release,
    ReverbAlgo, ReverbDamping, ReverbRoomSize, ReverbTime, StereoChannels, ThresholdDb, WetMix,
};
// `node_markers` is NOT glob-re-exported: its `ChorusNode` marker would collide
// with the `ChorusNode` DSP unit re-exported below. Reach markers via
// `tutti_units::node_markers::*`.

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

// Bevy ECS integration for the units domain — the audio-graph reconcile pieces
// that bind the DSP / spatial / convolution units into tutti's entity-as-node
// graph. Each duty is its own crate-root module carrying its own `Tutti*Plugin`
// (the bevy_text shape — no central `ecs` hub).
//
// The `spatial_graph` suffix disambiguates the graph-binding layer from the
// same-named `spatial/` DSP-unit module it drives (the panner nodes). The
// automation graph binding lives inside `automation::graph` alongside its DSP.
#[cfg(feature = "bevy")]
pub mod dsp;
#[cfg(feature = "bevy")]
pub mod reconcile;
#[cfg(feature = "bevy")]
pub use dsp::{spawn_dsp_node, spawn_lfo_nodes, AddDspNode, DspNode, SpawnParams, TuttiDspPlugin};
#[cfg(feature = "bevy")]
pub use reconcile::{
    bump_param_epoch_dsp, reconcile_reverb_params, reconcile_unit_params, EffectParams,
};

#[cfg(all(feature = "bevy", feature = "convolution"))]
pub mod pending_convolver;
#[cfg(all(feature = "bevy", feature = "convolution"))]
pub use pending_convolver::{
    promote_pending_convolvers, start_convolver_loads, PendingConvolverLoad,
};
#[cfg(all(feature = "bevy", feature = "convolution"))]
pub use reconcile::reconcile_convolver_params;

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
