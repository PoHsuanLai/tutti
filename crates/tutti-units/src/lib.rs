//! DSP nodes for the Tutti audio engine.
//!
//! # no_std
//!
//! `#![no_std]` when `std` feature is disabled. All DSP nodes work without std.

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(test)]
extern crate std;

mod error;
pub use error::{Error, Result};

pub use tutti_core::{
    params, Bpm, Cents, Db, Degrees, Hz, Linear, Param, Ratio, SampleRate, Seconds, Semitones, Unit,
};

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

/// Bevy ECS integration: spawn pipelines, param reconcilers, and plugins for
/// the DSP / spatial / automation / convolution units. Mirrors
/// `tutti_sampler::ecs`. Requires `std` (Bevy is std-only).
#[cfg(feature = "std")]
pub mod ecs;
// Re-export the ECS surface at the crate root so consumers write
// `tutti_units::TuttiDspPlugin`, not `tutti_units::ecs::…` (the bevy_text shape).
#[cfg(feature = "std")]
pub use ecs::{
    bump_param_epoch_dsp, reconcile_reverb_params, reconcile_unit_params, spawn_dsp_node,
    spawn_lfo_nodes, AddDspNode, DspNode, EffectParams, SpawnParams, TuttiDspPlugin,
};
#[cfg(all(feature = "std", feature = "convolution"))]
pub use ecs::{
    promote_pending_convolvers, reconcile_convolver_params, start_convolver_loads,
    PendingConvolverLoad,
};
#[cfg(all(feature = "std", feature = "spatial"))]
pub use ecs::{
    spatial_audio_sync_system, AttenuationModel, AudioListener, SpatialAudio, TuttiSpatialPlugin,
};
#[cfg(all(feature = "std", feature = "automation"))]
pub use ecs::{
    automation_lane_system, reconcile_automation_writes, update_automation_envelope_system,
    AddAutomationLane, AutomationDrivesParam, AutomationLaneEmitter, AutomationLaneNode,
    AutomationParam, TuttiAutomationPlugin, UpdateAutomationEnvelope,
};
