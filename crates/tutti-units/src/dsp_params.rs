//! Shared DSP parameter components — the cross-cutting scalar vocabulary that
//! the DSP node markers `#[require]` and the reconcile systems write through.
//!
//! These are deliberately a shared pool, not per-function: `WetMix` is used by
//! reverb / delay / chorus / convolution, `GainDb` by filter + compressor,
//! `Frequency` by filter + LFO, `ThresholdDb`/`Attack`/`Release` by compressor +
//! gate + limiter. They live in tutti-units (the crate whose reconcile systems
//! read them) rather than tutti-core, which keeps only the foundational graph
//! vocabulary (`Volume`/`Pan`/`Mute`/`PluginParam`/`NodeKind`/`AudioNode`).

use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;

// =============================================================================
// Construction-only authored data (no live `set()`)
//
// Read by the spawn systems only when building a unit — no per-frame reconcile,
// because the units have no live setter for these. `FilterMode`/`LfoShapeKind`
// mirror `tutti_units::{SvfType, LfoShape}` 1:1; the spawn systems map between
// the mirror and the real enum.
// =============================================================================

/// Whether a DSP node is built in stereo (`true`) or mono (`false`).
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[reflect(Component, Default)]
pub struct StereoChannels(pub bool);

/// Maximum delay-line length in seconds — sets the delay buffer size at
/// construction. Cannot change live (the buffer is fixed).
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component)]
pub struct MaxDelay(pub f32);

impl Default for MaxDelay {
    #[inline]
    fn default() -> Self {
        Self(4.0)
    }
}

/// Mirror of `tutti_units::SvfType`. Authored on a filter entity to pick the
/// SVF response at construction; the filter spawn system maps it to the real
/// `SvfType`.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[reflect(Component, Default)]
pub enum FilterMode {
    #[default]
    LowPass,
    HighPass,
    BandPass,
    Notch,
    Allpass,
    Bell,
    LowShelf,
    HighShelf,
}

/// Mirror of `tutti_units::LfoShape`. Authored on an LFO entity to pick the
/// waveform; the LFO spawn system maps it to the real `LfoShape`.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[reflect(Component, Default)]
pub enum LfoShapeKind {
    #[default]
    Sine,
    Triangle,
    Square,
    Sawtooth,
    SawtoothDown,
    Random,
    RandomSmooth,
}

/// Beat-sync flag for an LFO. When `true`, the LFO's `Frequency` is
/// interpreted as beats-per-cycle and it is wired to the transport.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[reflect(Component, Default)]
pub struct BeatSynced(pub bool);

// =============================================================================
// Filter / EQ params (NodeKind::Filter, NodeKind::Eq)
// =============================================================================

/// Cutoff / center frequency in Hz. Also used by LFO rate (beat-synced or Hz).
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component)]
pub struct Frequency(pub f32);

impl Default for Frequency {
    #[inline]
    fn default() -> Self {
        Self(1000.0)
    }
}

/// Filter Q / resonance. Higher = sharper / more resonant.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component)]
pub struct FilterQ(pub f32);

impl Default for FilterQ {
    #[inline]
    fn default() -> Self {
        Self(0.707)
    }
}

/// Gain in dB for shelves / bell EQs / makeup-gain stages.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Default)]
#[reflect(Component)]
pub struct GainDb(pub f32);

// =============================================================================
// Reverb params (NodeKind::Reverb)
// =============================================================================

/// Reverberation time to -60 dB, in seconds. Construction-only for
/// `reverb_stereo` (no live setter; reverb is crossfade-rebuilt).
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component)]
pub struct ReverbTime(pub f32);

impl Default for ReverbTime {
    #[inline]
    fn default() -> Self {
        Self(5.0)
    }
}

/// Reverb size — physical room size in meters for fundsp's `reverb_stereo`,
/// normalized 0..1 elsewhere. Reconcilers map through the unit's setter.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component)]
pub struct ReverbRoomSize(pub f32);

impl Default for ReverbRoomSize {
    #[inline]
    fn default() -> Self {
        Self(10.0)
    }
}

/// Reverb tail damping. Higher = more high-frequency absorption.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component)]
pub struct ReverbDamping(pub f32);

impl Default for ReverbDamping {
    #[inline]
    fn default() -> Self {
        Self(0.5)
    }
}

/// Which fundsp reverb opcode backs a `NodeKind::Reverb` node. Carried as a
/// component because the opcodes have no `set()` — the reverb reconciler reads
/// it to pick the constructor when crossfade-rebuilding. Mirrors
/// `dawai_types::ReverbAlgorithm`; the host maps between them.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[reflect(Component)]
pub enum ReverbAlgo {
    /// 32-channel FDN with damping (`reverb_stereo`).
    #[default]
    Fdn32,
    /// Optimized FDN (`reverb4_stereo`); damping is ignored.
    Fdn4,
}

/// Wet/dry mix. `0.0` = dry, `1.0` = fully wet. Shared by every wet effect
/// (reverb / delay / chorus / convolution).
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component)]
pub struct WetMix(pub f32);

impl Default for WetMix {
    #[inline]
    fn default() -> Self {
        Self(0.3)
    }
}

// =============================================================================
// Delay params (NodeKind::Delay)
// =============================================================================

/// Delay time in seconds (per channel for stereo delays).
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component)]
pub struct DelayTime(pub f32);

impl Default for DelayTime {
    #[inline]
    fn default() -> Self {
        Self(0.25)
    }
}

/// Delay feedback amount. `0.0` = single tap, `1.0` = self-oscillation.
/// Shared by delay + chorus/flanger/phaser.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component)]
pub struct Feedback(pub f32);

impl Default for Feedback {
    #[inline]
    fn default() -> Self {
        Self(0.4)
    }
}

// =============================================================================
// Chorus / modulation params (NodeKind::Chorus, …)
// =============================================================================

/// LFO rate in Hz for chorus / flanger / phaser modulators.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component)]
pub struct ModRate(pub f32);

impl Default for ModRate {
    #[inline]
    fn default() -> Self {
        Self(1.0)
    }
}

/// Modulation depth. Unit-defined range — typically seconds (delay modulation)
/// or normalized 0..1. Shared by chorus + LFO.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component)]
pub struct ModDepth(pub f32);

impl Default for ModDepth {
    #[inline]
    fn default() -> Self {
        Self(0.005)
    }
}

// =============================================================================
// Compressor / gate / limiter params
// =============================================================================

/// Threshold in dB for dynamics processors. Shared by compressor + gate.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component)]
pub struct ThresholdDb(pub f32);

impl Default for ThresholdDb {
    #[inline]
    fn default() -> Self {
        Self(-20.0)
    }
}

/// Compression ratio. `1.0` = no compression, `4.0` = 4:1, `f32::INFINITY` = limiter.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component)]
pub struct CompressorRatio(pub f32);

impl Default for CompressorRatio {
    #[inline]
    fn default() -> Self {
        Self(4.0)
    }
}

/// Attack time in seconds. Shared by compressor + gate.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component)]
pub struct Attack(pub f32);

impl Default for Attack {
    #[inline]
    fn default() -> Self {
        Self(0.005)
    }
}

/// Release time in seconds. Shared by compressor + gate.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component)]
pub struct Release(pub f32);

impl Default for Release {
    #[inline]
    fn default() -> Self {
        Self(0.1)
    }
}

/// Output ceiling in dB. Used by limiters to cap output level.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component)]
pub struct CeilingDb(pub f32);

impl Default for CeilingDb {
    #[inline]
    fn default() -> Self {
        Self(-0.3)
    }
}

// =============================================================================
// Ladder filter param (NodeKind::Ladder)
// =============================================================================

/// Drive / saturation amount for the ladder filter. `1.0` is unity (no extra
/// saturation), higher values overdrive the feedback path.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component)]
pub struct Drive(pub f32);

impl Default for Drive {
    #[inline]
    fn default() -> Self {
        Self(1.0)
    }
}

// =============================================================================
// Spatial panner params (NodeKind::SpatialPanner)
// =============================================================================

/// Azimuth angle in degrees. 0=front, 90=left, -90=right, ±180=rear.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Default)]
#[reflect(Component)]
pub struct Azimuth(pub f32);

/// Elevation angle in degrees. 0=ear level, +90=above, -90=below.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Default)]
#[reflect(Component)]
pub struct Elevation(pub f32);

/// Register every shared param component for reflection. Called by
/// `TuttiDspPlugin`. Idempotent — Bevy ignores duplicate `register_type`.
pub fn register_param_types(app: &mut bevy_app::App) {
    app.register_type::<StereoChannels>()
        .register_type::<MaxDelay>()
        .register_type::<FilterMode>()
        .register_type::<LfoShapeKind>()
        .register_type::<BeatSynced>()
        .register_type::<Frequency>()
        .register_type::<FilterQ>()
        .register_type::<GainDb>()
        .register_type::<ReverbTime>()
        .register_type::<ReverbRoomSize>()
        .register_type::<ReverbDamping>()
        .register_type::<ReverbAlgo>()
        .register_type::<WetMix>()
        .register_type::<DelayTime>()
        .register_type::<Feedback>()
        .register_type::<ModRate>()
        .register_type::<ModDepth>()
        .register_type::<ThresholdDb>()
        .register_type::<CompressorRatio>()
        .register_type::<Attack>()
        .register_type::<Release>()
        .register_type::<CeilingDb>()
        .register_type::<Drive>()
        .register_type::<Azimuth>()
        .register_type::<Elevation>();
}
