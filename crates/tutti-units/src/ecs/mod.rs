//! Bevy ECS integration for the DSP / automation / spatial units.
//!
//! The units-domain half of bevy-tutti's audio-graph reconciler, folded into
//! the leaf crate so the Components / Systems / Plugins live next to the audio
//! logic they drive. The generic reconcile hub (`GraphReconcileSystems`,
//! `SpawnAudioNode`, `engine_ready`, `GraphDirty`, `reconcile_params`,
//! `commit_graph`, the `NodeParamEpoch` resource, …) lives in
//! [`tutti_core::ecs`]; this module layers the units-specific pieces on top:
//!
//! - [`dsp`] — DSP spawn pipeline + the `TuttiDspPlugin` (unconditional; DSP
//!   units are core).
//! - [`reconcile`] — DSP param reconcilers (`reconcile_unit_params` /
//!   `reconcile_reverb_params`), the convolver reconciler (`convolution`-gated),
//!   and the DSP param-epoch bump.
//! - [`spatial`] — 3D panning + `TuttiSpatialPlugin` (`spatial`-gated).
//! - [`automation`] — automation-lane spawn + binding + `TuttiAutomationPlugin`
//!   (`automation`-gated).
//! - [`pending_convolver`] — deferred IR load → convolver node
//!   (`convolution`-gated).

pub mod dsp;
pub mod reconcile;

#[cfg(feature = "spatial")]
pub mod spatial;

#[cfg(feature = "automation")]
pub mod automation;

#[cfg(feature = "convolution")]
pub mod pending_convolver;

pub use dsp::{
    dsp_chorus_system, dsp_compressor_system, dsp_delay_system, dsp_filter_system, dsp_gate_system,
    dsp_lfo_system, dsp_reverb_system, spawn_dsp_node, spawn_lfo_nodes, AddDspNode, DspNode,
    SpawnParams, TuttiDspPlugin,
};
#[allow(deprecated)]
pub use dsp::{AddChorus, AddCompressor, AddDelay, AddFilter, AddGate, AddLfo, AddReverb};

pub use reconcile::{
    bump_param_epoch_dsp, reconcile_reverb_params, reconcile_unit_params, EffectParams,
};

#[cfg(feature = "convolution")]
pub use reconcile::reconcile_convolver_params;

#[cfg(feature = "spatial")]
pub use spatial::{
    spatial_audio_sync_system, AttenuationModel, AudioListener, SpatialAudio, TuttiSpatialPlugin,
};

#[cfg(feature = "automation")]
pub use automation::{
    automation_lane_system, reconcile_automation_writes, update_automation_envelope_system,
    AddAutomationLane, AutomationDrivesParam, AutomationLaneEmitter, AutomationLaneNode,
    AutomationParam, TuttiAutomationPlugin, UpdateAutomationEnvelope,
};

#[cfg(feature = "convolution")]
pub use pending_convolver::{
    promote_pending_convolvers, start_convolver_loads, PendingConvolverLoad,
};
