//! Transport state shared with the plugin each process block.
//!
//! Superset of the fields VST2, VST3, and CLAP plugins consume. Formats
//! that don't surface a given field leave it at its `Default`.
//!
//! Fields are grouped into focused sub-structs ([`TransportFlags`],
//! [`MusicalTiming`], [`TransportPosition`], [`LoopRegion`], [`BarInfo`])
//! so the top-level type stays readable; callers access via
//! `transport.state.playing`, `transport.timing.tempo`, etc.

/// Transport snapshot passed into a plugin's process call.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TransportInfo {
    pub state: TransportFlags,
    pub timing: MusicalTiming,
    pub position: TransportPosition,
    pub loop_region: LoopRegion,
    pub bar: BarInfo,
    /// Sample rate in Hz (vst3 `ProcessContext::sampleRate`).
    pub sample_rate: f64,
}

/// Playback / record / cycle flags.
///
/// Named `TransportFlags` after the plugin-SDK term for exactly this bundle —
/// CLAP and VST2 both call the field `flags`. Distinct from the
/// [`TransportState`](tutti_core::transport::TransportState) *trait*, which is
/// the live-timeline reader plugins consume; this is the wire snapshot.
#[derive(Debug, Clone, Copy, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TransportFlags {
    pub playing: bool,
    pub recording: bool,
    pub cycle_active: bool,
}

/// Musical timing — tempo and time signature.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct MusicalTiming {
    pub tempo: f64,
    pub time_sig_numerator: i32,
    pub time_sig_denominator: i32,
}

impl Default for MusicalTiming {
    /// 120 BPM, 4/4 — the conventional musical defaults.
    fn default() -> Self {
        Self {
            tempo: 120.0,
            time_sig_numerator: 4,
            time_sig_denominator: 4,
        }
    }
}

/// Play head position in three coordinate systems. Hosts populate
/// whichever the underlying plugin format understands; format-specific
/// bridge code reads only the fields it needs.
#[derive(Debug, Clone, Copy, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TransportPosition {
    /// Sample-accurate project timeline position (vst2 `samplePos`; vst3
    /// `projectTimeSamples`). Jumps when the transport loops/relocates.
    pub samples: i64,
    /// Monotonic sample counter that does **not** reset on loop/cycle (vst3
    /// `continousTimeSamples`; clap `steady_time`). Free-running plugins (LFOs,
    /// delays) key their timing off this. `0` means "host has no separate
    /// continuous clock" — consumers fall back to [`samples`](Self::samples).
    pub continuous_samples: i64,
    /// Quarter notes from project start (vst2 `ppqPos`; vst3
    /// `projectTimeMusic`).
    pub quarters: f64,
    /// Beats from song start (clap `song_pos_beats`). In 4/4 this
    /// matches `quarters`; in other time signatures the host is
    /// responsible for the conversion.
    pub beats: f64,
    /// Seconds from song start (clap `song_pos_seconds`).
    pub seconds: f64,
}

/// Cycle / loop region boundaries in both coordinate systems.
#[derive(Debug, Clone, Copy, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct LoopRegion {
    /// Cycle start in quarter notes (vst2/vst3).
    pub start_quarters: f64,
    /// Cycle end in quarter notes (vst2/vst3).
    pub end_quarters: f64,
    /// Cycle start in beats (clap `loop_start_beats`).
    pub start_beats: f64,
    /// Cycle end in beats (clap `loop_end_beats`).
    pub end_beats: f64,
}

/// Current-bar metadata.
#[derive(Debug, Clone, Copy, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BarInfo {
    /// Position of the current bar in quarter notes (vst2/vst3
    /// `barPositionMusic`).
    pub position_quarters: f64,
    /// Position of the current bar in beats (clap `bar_start`).
    pub start_beats: f64,
    /// 1-based bar number (clap `bar_number`).
    pub number: i32,
}

impl Default for TransportInfo {
    /// Defaults to a stopped transport at 120 BPM, 4/4 — musical defaults
    /// that consumers expect when no transport state has been negotiated yet.
    fn default() -> Self {
        Self {
            state: TransportFlags::default(),
            timing: MusicalTiming::default(),
            position: TransportPosition::default(),
            loop_region: LoopRegion::default(),
            bar: BarInfo::default(),
            sample_rate: 0.0,
        }
    }
}

impl TransportInfo {
    /// Alias for [`Default::default`].
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_tempo(mut self, tempo: f64) -> Self {
        self.timing.tempo = tempo;
        self
    }

    pub fn with_playing(mut self, playing: bool) -> Self {
        self.state.playing = playing;
        self
    }

    pub fn with_recording(mut self, recording: bool) -> Self {
        self.state.recording = recording;
        self
    }

    pub fn with_time_signature(mut self, numerator: i32, denominator: i32) -> Self {
        self.timing.time_sig_numerator = numerator;
        self.timing.time_sig_denominator = denominator;
        self
    }

    pub fn with_sample_rate(mut self, sample_rate: f64) -> Self {
        self.sample_rate = sample_rate;
        self
    }

    /// Set CLAP-style position (beats + seconds).
    pub fn with_position_beats(mut self, beats: f64, seconds: f64) -> Self {
        self.position.beats = beats;
        self.position.seconds = seconds;
        self
    }

    /// Set VST-style position (quarter notes + free-running sample count).
    pub fn with_position_quarters(mut self, quarters: f64, samples: i64) -> Self {
        self.position.quarters = quarters;
        self.position.samples = samples;
        self
    }

    /// Set the monotonic continuous sample counter (vst3 `continousTimeSamples`
    /// / clap `steady_time`) — the one that does not reset on loop. Leave unset
    /// (0) and consumers fall back to the project-time `samples`.
    pub fn with_continuous_samples(mut self, continuous_samples: i64) -> Self {
        self.position.continuous_samples = continuous_samples;
        self
    }

    pub fn with_bar(mut self, start_beats: f64, number: i32) -> Self {
        self.bar.start_beats = start_beats;
        self.bar.number = number;
        self
    }

    /// CLAP-style loop region (beats + cycle-active flag).
    pub fn with_loop(mut self, active: bool, start_beats: f64, end_beats: f64) -> Self {
        self.state.cycle_active = active;
        self.loop_region.start_beats = start_beats;
        self.loop_region.end_beats = end_beats;
        self
    }
}
