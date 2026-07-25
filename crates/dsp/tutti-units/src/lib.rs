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
    params, Amplitude, ArcDegrees, Azimuth, Bpm, Cents, CompressionRatio, Db, Depth, Drive,
    Elevation, Feedback, Hz, Mix, Param, Resonance, SampleRate, Seconds, Semitones, Spread,
    StereoWidth, Unit, Q,
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
// `LfoNode` is now `ModulatorNode<Lfo>` — the fundsp adapter over a pure
// `tutti_mod::Modulator`. `LfoShape`/`Lfo`/`Modulator` are re-exported from
// `tutti-mod` through `lfo` so existing `use tutti_units::LfoShape` sites are
// untouched.
pub use lfo::{Lfo, LfoMode, LfoNode, LfoShape, Modulator, ModulatorNode};

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

// The native `ModParams` impls (the trait itself lives in tutti-mod).
mod mod_params;

// Re-export the `ModParams` trait + modulation *target* surface from tutti-mod so
// downstream crates (e.g. tutti-plugin implementing `ModParams`) reach it here
// alongside `Lfo`, without a separate tutti-mod dep. The routing feature is on
// (tutti-units deps tutti-mod with `features = ["routing"]`).
pub use tutti_mod::{AtomicTarget, LayerKey, LayeredCurve, ModParams, ModTarget};

#[cfg(feature = "spatial")]
mod spatial;
#[cfg(feature = "spatial")]
pub use spatial::{build_surround_mix, ChannelSumUnit, SpatialPannerNode, SurroundSource};
#[cfg(feature = "hrtf")]
pub use spatial::{HrtfBinauralError, HrtfBinauralNode};

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
/// `AudioUnit`, the [`Curve`](automation::Curve) trait it evaluates, and the
/// recording-side `Manager`/`Recorder`. See the module docs.
pub mod automation;

// NOTE: the spatial-panner graph binding (`spatial_graph`) and the automation
// graph binding (`automation::graph`) moved app-side to
// `dawai_model::engine_bind::{spatial, automation}` — they bound the DAW
// `Volume`/`Pan`/`PluginParam` components, which left the engine. This crate
// keeps only the pure DSP: the spatial panner nodes (`spatial/`) + the
// automation `AudioUnit` (`automation::{lane, recording}`).
