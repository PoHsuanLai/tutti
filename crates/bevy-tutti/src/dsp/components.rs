//! `Add*` trigger components for DSP units.

use bevy_ecs::prelude::*;

/// Trigger component: spawn an entity with this to add a compressor to the graph.
///
/// The `dsp_compressor_system` processes entities with `Added<AddCompressor>`,
/// creates a `Compressor` (mono or stereo via `Compressor::mono`/`Compressor::stereo`),
/// adds it to the graph, and inserts `AudioEmitter`.
#[cfg(feature = "dsp")]
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct AddCompressor {
    pub threshold_db: f32,
    pub ratio: f32,
    pub attack: f32,
    pub release: f32,
    pub makeup_db: f32,
    pub stereo: bool,
}

#[cfg(feature = "dsp")]
impl Default for AddCompressor {
    fn default() -> Self {
        Self {
            threshold_db: -20.0,
            ratio: 4.0,
            attack: 0.005,
            release: 0.1,
            makeup_db: 0.0,
            stereo: false,
        }
    }
}

#[cfg(feature = "dsp")]
impl AddCompressor {
    pub fn new(threshold_db: f32, ratio: f32) -> Self {
        Self {
            threshold_db,
            ratio,
            ..Default::default()
        }
    }

    pub fn attack(mut self, seconds: f32) -> Self {
        self.attack = seconds;
        self
    }

    pub fn release(mut self, seconds: f32) -> Self {
        self.release = seconds;
        self
    }

    pub fn makeup(mut self, db: f32) -> Self {
        self.makeup_db = db;
        self
    }

    pub fn stereo(mut self) -> Self {
        self.stereo = true;
        self
    }
}

/// Trigger component: spawn an entity with this to add a gate to the graph.
///
/// The `dsp_gate_system` processes entities with `Added<AddGate>`,
/// creates a `Gate` (mono or stereo via `Gate::mono`/`Gate::stereo`), adds it to
/// the graph, and inserts `AudioEmitter`.
#[cfg(feature = "dsp")]
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct AddGate {
    pub threshold_db: f32,
    pub attack: f32,
    pub hold: f32,
    pub release: f32,
    pub stereo: bool,
}

#[cfg(feature = "dsp")]
impl Default for AddGate {
    fn default() -> Self {
        Self {
            threshold_db: -30.0,
            attack: 0.001,
            hold: 0.01,
            release: 0.1,
            stereo: false,
        }
    }
}

#[cfg(feature = "dsp")]
impl AddGate {
    pub fn new(threshold_db: f32) -> Self {
        Self {
            threshold_db,
            ..Default::default()
        }
    }

    pub fn attack(mut self, seconds: f32) -> Self {
        self.attack = seconds;
        self
    }

    pub fn hold(mut self, seconds: f32) -> Self {
        self.hold = seconds;
        self
    }

    pub fn release(mut self, seconds: f32) -> Self {
        self.release = seconds;
        self
    }

    pub fn stereo(mut self) -> Self {
        self.stereo = true;
        self
    }
}

/// Trigger component: spawn an entity with this to add an LFO to the graph.
///
/// The `dsp_lfo_system` processes entities with `Added<AddLfo>`,
/// creates an `LfoNode`, adds it to the graph, and inserts `AudioEmitter`.
/// If `beat_synced` is true, the LFO is wired to the engine's transport.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct AddLfo {
    pub shape: tutti::units::LfoShape,
    pub frequency: f32,
    pub depth: f32,
    pub beat_synced: bool,
}

impl Default for AddLfo {
    fn default() -> Self {
        Self {
            shape: tutti::units::LfoShape::Sine,
            frequency: 1.0,
            depth: 1.0,
            beat_synced: false,
        }
    }
}

impl AddLfo {
    /// Free-running LFO with the given shape and frequency in Hz.
    pub fn new(shape: tutti::units::LfoShape, frequency: f32) -> Self {
        Self {
            shape,
            frequency,
            ..Default::default()
        }
    }

    /// Beat-synced LFO with the given shape and beats per cycle.
    pub fn beat_synced(shape: tutti::units::LfoShape, beats_per_cycle: f32) -> Self {
        Self {
            shape,
            frequency: beats_per_cycle,
            beat_synced: true,
            ..Default::default()
        }
    }

    pub fn depth(mut self, depth: f32) -> Self {
        self.depth = depth;
        self
    }
}

/// Trigger component: spawn an entity with this to add a stereo SVF filter.
///
/// `dsp_filter_system` consumes `Added<AddFilter>`, builds a
/// `StereoSvfFilterNode<f64>`, attaches `AudioNode`, `NodeKind::Filter`,
/// and the `Frequency` / `FilterQ` / `GainDb` param components, then
/// removes the trigger.
#[cfg(feature = "dsp")]
#[derive(Component, Debug, Clone, Copy)]
pub struct AddFilter {
    pub svf_type: tutti::units::SvfType,
    pub frequency: f32,
    pub q: f32,
    /// Only used for Bell / LowShelf / HighShelf modes.
    pub gain_db: f32,
}

#[cfg(feature = "dsp")]
impl Default for AddFilter {
    fn default() -> Self {
        Self {
            svf_type: tutti::units::SvfType::LowPass,
            frequency: 1000.0,
            q: 0.707,
            gain_db: 0.0,
        }
    }
}

#[cfg(feature = "dsp")]
impl AddFilter {
    pub fn lowpass(frequency: f32, q: f32) -> Self {
        Self {
            svf_type: tutti::units::SvfType::LowPass,
            frequency,
            q,
            gain_db: 0.0,
        }
    }
    pub fn highpass(frequency: f32, q: f32) -> Self {
        Self {
            svf_type: tutti::units::SvfType::HighPass,
            frequency,
            q,
            gain_db: 0.0,
        }
    }
    pub fn bandpass(frequency: f32, q: f32) -> Self {
        Self {
            svf_type: tutti::units::SvfType::BandPass,
            frequency,
            q,
            gain_db: 0.0,
        }
    }
    pub fn notch(frequency: f32, q: f32) -> Self {
        Self {
            svf_type: tutti::units::SvfType::Notch,
            frequency,
            q,
            gain_db: 0.0,
        }
    }
    pub fn bell(frequency: f32, q: f32, gain_db: f32) -> Self {
        Self {
            svf_type: tutti::units::SvfType::Bell,
            frequency,
            q,
            gain_db,
        }
    }
    pub fn low_shelf(frequency: f32, q: f32, gain_db: f32) -> Self {
        Self {
            svf_type: tutti::units::SvfType::LowShelf,
            frequency,
            q,
            gain_db,
        }
    }
    pub fn high_shelf(frequency: f32, q: f32, gain_db: f32) -> Self {
        Self {
            svf_type: tutti::units::SvfType::HighShelf,
            frequency,
            q,
            gain_db,
        }
    }
}

/// Trigger component: spawn an entity with this to add stereo reverb.
///
/// fundsp's `reverb_stereo` is built fresh per AddReverb and does not
/// expose post-construction setters; live wet/room/damping changes
/// require respawning the node (the reconciler handles this).
#[cfg(feature = "dsp")]
#[derive(Component, Debug, Clone, Copy)]
pub struct AddReverb {
    /// Room size in meters (10..30 typical).
    pub room_size: f32,
    /// Reverberation time to -60 dB, in seconds.
    pub time_secs: f32,
    /// Tail damping (0..1).
    pub damping: f32,
    /// Wet/dry mix (0..1).
    pub wet: f32,
}

#[cfg(feature = "dsp")]
impl Default for AddReverb {
    fn default() -> Self {
        Self {
            room_size: 10.0,
            time_secs: 5.0,
            damping: 0.5,
            wet: 0.3,
        }
    }
}

/// Trigger component: spawn an entity with this to add a stereo delay.
#[cfg(feature = "dsp")]
#[derive(Component, Debug, Clone, Copy)]
pub struct AddDelay {
    /// Maximum delay length in seconds — sets the buffer size.
    pub max_delay_secs: f32,
    /// Initial delay time, applied to both L and R.
    pub delay_time_secs: f32,
    pub feedback: f32,
    pub wet: f32,
}

#[cfg(feature = "dsp")]
impl Default for AddDelay {
    fn default() -> Self {
        Self {
            max_delay_secs: 4.0,
            delay_time_secs: 0.25,
            feedback: 0.4,
            wet: 0.3,
        }
    }
}

/// Trigger component: spawn an entity with this to add a stereo chorus.
#[cfg(feature = "dsp")]
#[derive(Component, Debug, Clone, Copy)]
pub struct AddChorus {
    pub rate_hz: f32,
    pub depth_secs: f32,
    pub feedback: f32,
    pub wet: f32,
}

#[cfg(feature = "dsp")]
impl Default for AddChorus {
    fn default() -> Self {
        Self {
            rate_hz: 1.0,
            depth_secs: 0.005,
            feedback: 0.3,
            wet: 0.5,
        }
    }
}
