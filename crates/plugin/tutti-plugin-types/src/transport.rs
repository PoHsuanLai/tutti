//! Transport state shared with the plugin each process block.
//!
//! Superset of the fields VST2, VST3, and CLAP plugins consume. Formats
//! that don't surface a given field leave it at its `Default`.
//!
//! Fields are grouped into focused sub-structs ([`TransportFlags`],
//! [`MusicalTiming`], [`TransportPosition`], [`LoopRegion`], [`BarInfo`])
//! so the top-level type stays readable; callers access via
//! `transport.state.playing`, `transport.timing.tempo`, etc.
//!
//! Musical quantities are carried as their engine types ([`TimeSignature`],
//! [`BarNumber`]) rather than as loose integers, and each format host converts at
//! its own boundary via `From` — the same shape `ChannelLayout` uses for speaker
//! arrangements.

use tutti_types::meter::{BarNumber, TimeSignature};

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

/// Whether a transport `f64` is worth advertising to a plugin as valid.
///
/// Every plugin API has per-field "this value is filled in" flags, and setting
/// one for a NaN or an infinity is worse than leaving it clear: a plugin that
/// trusts the flag does arithmetic with the value, and NaN propagates straight
/// through its timing math into the audio buffer. A cleared flag makes the
/// plugin fall back to its own defaults, which is always recoverable.
///
/// Lives here rather than in one format host because every format needs the
/// same gate and they had drifted: the VST2 path checked values, while the VST3
/// path set `kTempoValid` from the plugin's requirement mask alone — so a NaN
/// or zero tempo reached VST3 plugins flagged valid.
///
/// This is the finiteness half only. A field with an additional domain rule
/// (tempo must also be positive) applies that at the call site, since the rule
/// is per-field rather than per-type.
pub fn is_usable(value: f64) -> bool {
    value.is_finite()
}

/// Playback / record / cycle flags.
///
/// Named `TransportFlags` after the plugin-SDK term for exactly this bundle —
/// CLAP and VST2 both call the field `flags`. Distinct from tutti-core's
/// `TransportState` *trait* (the live-timeline reader plugins consume); this is
/// the wire snapshot.
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
    /// The signature in force at the playhead.
    ///
    /// A [`TimeSignature`] rather than a loose `(i32, i32)` pair: each format
    /// wants a different width (CLAP `u16`, VST2/VST3 `i32`), and casting at
    /// three separate boundaries is how the CLAP bridge ended up doing
    /// `as u16` on a signed value — a negative numerator became 65535. The
    /// conversions now live on the type and validate on the way through.
    pub signature: TimeSignature,
}

impl Default for MusicalTiming {
    /// 120 BPM, 4/4 — the conventional musical defaults.
    fn default() -> Self {
        Self {
            tempo: 120.0,
            signature: TimeSignature::default(),
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
    ///
    /// `None` means the host has no project-time sample clock to report. It is
    /// an `Option` rather than a plain `i64` because no producer in this engine
    /// fills it: the transport's authority is musical (beats), and deriving
    /// project-time samples from beats and tempo is wrong the moment tempo
    /// moves — see `tutti-plugin`'s `TransportSource`. Neither the VST2 nor the
    /// VST3 ABI has a validity bit for their sample-position field, so a plain
    /// `0` was forwarded as fact and every plugin doing sample-accurate math
    /// saw the project frozen at sample 0 forever. The `Option` forces each
    /// format host to decide what to send instead of silently forwarding a
    /// placeholder.
    pub samples: Option<i64>,
    /// Monotonic sample counter that does **not** reset on loop/cycle (vst3
    /// `continousTimeSamples`; clap `steady_time`). Free-running plugins (LFOs,
    /// delays) key their timing off this. `0` means "host has no separate
    /// continuous clock" — consumers fall back to [`samples`](Self::samples),
    /// which is itself optional.
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
    pub number: BarNumber,
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

    pub fn with_time_signature(mut self, signature: TimeSignature) -> Self {
        self.timing.signature = signature;
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

    /// Set VST-style musical position (quarter notes).
    ///
    /// Deliberately does **not** take a sample position: the two are not
    /// derivable from one another once tempo moves. A host that genuinely has a
    /// project-time sample clock reports it separately via
    /// [`with_position_samples`](Self::with_position_samples); one that doesn't
    /// leaves [`TransportPosition::samples`] `None`.
    pub fn with_position_quarters(mut self, quarters: f64) -> Self {
        self.position.quarters = quarters;
        self
    }

    /// Set the project-time sample position (vst2 `samplePos`; vst3
    /// `projectTimeSamples`) — the one that jumps on loop/relocate.
    ///
    /// Only call this if the host actually tracks project time in samples. No
    /// producer in this engine does; see [`TransportPosition::samples`].
    pub fn with_position_samples(mut self, samples: i64) -> Self {
        self.position.samples = Some(samples);
        self
    }

    /// Set the monotonic continuous sample counter (vst3 `continousTimeSamples`
    /// / clap `steady_time`) — the one that does not reset on loop. Leave unset
    /// (0) and consumers fall back to the project-time `samples`.
    pub fn with_continuous_samples(mut self, continuous_samples: i64) -> Self {
        self.position.continuous_samples = continuous_samples;
        self
    }

    /// Set every bar field.
    ///
    /// `position_quarters` and `start_beats` take the same value because the
    /// engine measures both in quarter notes — the distinction exists only for
    /// hosts whose CLAP beat axis is notated beats rather than quarters.
    /// Populating both matters: VST2's `bar_start_pos` and VST3's
    /// `barPositionMusic` read `position_quarters`, which the previous
    /// `with_bar` left at zero, so every VST plugin saw bar 0 at position 0.
    pub fn with_bar(mut self, start_quarters: f64, number: BarNumber) -> Self {
        self.bar.position_quarters = start_quarters;
        self.bar.start_beats = start_quarters;
        self.bar.number = number;
        self
    }

    /// Set every loop field, plus the cycle-active flag.
    ///
    /// The `_beats` and `_quarters` pairs take the same value for the same
    /// reason [`with_bar`](Self::with_bar) does: the engine measures both in
    /// quarter notes, and the distinction exists only for hosts whose CLAP beat
    /// axis is notated beats rather than quarters. Populating both matters:
    /// VST2's `cycle_start_pos`/`cycle_end_pos` and VST3's
    /// `cycleStartMusic`/`cycleEndMusic` read the `_quarters` pair, which the
    /// previous `with_loop` left at zero — and both hosts set their
    /// cycle-valid bit off `active` regardless, so every VST plugin was told a
    /// 0..0 loop region was real.
    pub fn with_loop(mut self, active: bool, start_beats: f64, end_beats: f64) -> Self {
        self.state.cycle_active = active;
        self.loop_region.start_beats = start_beats;
        self.loop_region.end_beats = end_beats;
        self.loop_region.start_quarters = start_beats;
        self.loop_region.end_quarters = end_beats;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `with_loop` used to fill only the `_beats` pair, so CLAP saw the loop and
    /// VST2/VST3 — which read `_quarters` — saw 0..0 while their cycle-valid bit
    /// was set anyway. Both pairs must be populated, exactly as `with_bar` does.
    #[test]
    fn with_loop_fills_quarters_as_well_as_beats() {
        let t = TransportInfo::new().with_loop(true, 4.0, 16.0);

        assert!(t.state.cycle_active);
        assert_eq!(t.loop_region.start_beats, 4.0);
        assert_eq!(t.loop_region.end_beats, 16.0);
        assert_eq!(t.loop_region.start_quarters, 4.0);
        assert_eq!(t.loop_region.end_quarters, 16.0);
    }

    /// The project-time sample clock has no producer, so it must arrive as
    /// `None` — not as a `0` that VST2/VST3 forward as fact.
    #[test]
    fn position_samples_is_absent_until_a_host_reports_one() {
        assert_eq!(TransportInfo::new().position.samples, None);
        assert_eq!(
            TransportInfo::new()
                .with_position_quarters(4.0)
                .position
                .samples,
            None
        );
        assert_eq!(
            TransportInfo::new()
                .with_position_samples(1_234)
                .position
                .samples,
            Some(1_234)
        );
    }
}
