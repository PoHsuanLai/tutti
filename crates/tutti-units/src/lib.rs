//! DSP nodes for the Tutti audio engine.

mod error;
pub use error::{Error, Result};

mod node_id;

pub use tutti_core::{
    params, Bpm, Cents, Db, Degrees, Hz, Linear, Param, Ratio, SampleRate, Seconds, Semitones, Unit,
};

// The shared DSP parameter pool (Frequency, FilterQ, GainDb, WetMix, …) and the
// node authoring markers — the cross-cutting ECS vocabulary the reconcile/spawn
// systems below read. Moved out of tutti-core (which keeps only the foundational
// graph params) so the API sits in the crate whose DSP it drives.
pub mod dsp_params;
pub mod node_markers;
pub use dsp_params::{
    Attack, Azimuth, BeatSynced, CeilingDb, CompressorRatio, DelayTime, Drive, Elevation, Feedback,
    FilterMode, FilterQ, Frequency, GainDb, LfoShapeKind, MaxDelay, ModDepth, ModRate, Release,
    ReverbAlgo, ReverbDamping, ReverbRoomSize, ReverbTime, StereoChannels, ThresholdDb, WetMix,
};
// `node_markers` is NOT glob-re-exported: its `ChorusNode` marker would collide
// with the `ChorusNode` DSP unit re-exported below. Reach markers via
// `tutti_units::node_markers::*`.

pub mod buffer;
pub mod coeff_cache;
pub mod smoothing;

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

/// Transport-driven envelope nodes. Reads beat position from a
/// `TransportReader` and emits a control signal sample-per-sample.
///
/// Was previously its own `tutti-automation` crate; folded in once the
/// only-uses-it consumer (`AutomationLane` as an `AudioUnit`) made the
/// extra workspace member pointless. Envelope primitives still come
/// from the `audio_automation` crate — re-exported here so consumers
/// only need one import path.
///
/// Both halves of automation live here: the playback-side [`AutomationLane`]
/// and the recording-side [`Manager`]/[`Recorder`]/[`RecordingTarget`]
/// (write/touch/latch capture during a take).
#[cfg(feature = "automation")]
pub mod automation {
    pub use crate::automation_lane::{AutomationLane, LiveAutomationLane};
    pub use crate::automation_recording::{
        AutomationRecordingConfig, AutomationSnapshot, AutomationTarget, Manager, Recorder,
        RecordingTarget,
    };

    pub use audio_automation::{
        AutomationClip, AutomationEnvelope, AutomationPoint, AutomationState, CurveType,
    };
}

#[cfg(feature = "automation")]
mod automation_lane;

#[cfg(feature = "automation")]
mod automation_recording;

// Bevy ECS integration for the units domain — the audio-graph reconcile pieces
// that bind the DSP / spatial / automation / convolution units into tutti's
// entity-as-node graph. Each duty is its own crate-root module carrying its own
// `Tutti*Plugin` (the bevy_text shape — no central `ecs` hub).
//
// `*_graph` suffixes disambiguate the graph-binding layer from the same-named
// DSP-unit module it drives (`spatial/` the panner nodes vs `spatial_graph` the
// reconciler; `automation_lane`/`automation_recording` the DSP vs
// `automation_graph` the ECS lane binding).
pub mod dsp;
pub mod reconcile;
pub use dsp::{spawn_dsp_node, spawn_lfo_nodes, AddDspNode, DspNode, SpawnParams, TuttiDspPlugin};
pub use reconcile::{bump_param_epoch_dsp, reconcile_reverb_params, reconcile_unit_params, EffectParams};

#[cfg(feature = "convolution")]
pub mod pending_convolver;
#[cfg(feature = "convolution")]
pub use pending_convolver::{promote_pending_convolvers, start_convolver_loads, PendingConvolverLoad};
#[cfg(feature = "convolution")]
pub use reconcile::reconcile_convolver_params;

#[cfg(feature = "spatial")]
pub mod spatial_graph;
#[cfg(feature = "spatial")]
pub use spatial_graph::{
    spatial_audio_sync_system, AttenuationModel, AudioListener, SpatialAudio, TuttiSpatialPlugin,
};

#[cfg(feature = "automation")]
pub mod automation_graph;
#[cfg(feature = "automation")]
pub use automation_graph::{
    automation_lane_system, reconcile_automation_writes, update_automation_envelope_system,
    AddAutomationLane, AutomationDrivesParam, AutomationLaneEmitter, AutomationLaneNode,
    AutomationParam, TuttiAutomationPlugin, UpdateAutomationEnvelope,
};
