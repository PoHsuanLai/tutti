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

/// A transport snapshot passed into a plugin's process call.
///
/// The default is a stopped transport at 120 BPM, 4/4, with a `sample_rate` of
/// `0.0`; the `with_*` builders fill in the rest.
///
/// # Examples
///
/// ```
/// use tutti_plugin_types::{BarNumber, TransportInfo};
///
/// let info = TransportInfo::new()
///     .with_sample_rate(48_000.0)
///     .with_tempo(128.0)
///     .with_playing(true)
///     .with_position_quarters(16.0)
///     .with_bar(16.0, BarNumber::default());
/// assert!(info.state.playing);
/// assert_eq!(info.position.samples, None); // the producer reports it separately
/// ```
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TransportInfo {
    /// Playing / recording / cycling.
    pub state: TransportFlags,
    /// Tempo and time signature at the playhead.
    pub timing: MusicalTiming,
    /// Playhead position, in whichever coordinate systems the host fills.
    pub position: TransportPosition,
    /// Cycle boundaries. Meaningful only when
    /// [`TransportFlags::cycle_active`] is set.
    pub loop_region: LoopRegion,
    /// Where the current bar starts, and its number.
    pub bar: BarInfo,
    /// Sample rate in Hz (vst3 `ProcessContext::sampleRate`). A raw `f64`
    /// rather than `Hz` because it is handed straight to a C ABI, which is
    /// where the unit types stop — and `Hz` is `f32`-backed, so it could not
    /// carry the ABI's value verbatim anyway.
    pub sample_rate: f64,
    /// The host's monotonic clock at the snapshot's first frame, in
    /// nanoseconds on the OS's monotonic clock (`CLOCK_MONOTONIC`; macOS
    /// `CLOCK_UPTIME_RAW`, Core Audio's host time; Windows QPC), the clock
    /// plugins compare it against (vst3 `systemTime`; vst2 `nanoSeconds`).
    ///
    /// `None` when the producer has no clock that relates to the audio: an
    /// offline render runs faster or slower than real time, and a hand-built
    /// snapshot has none. A raw `i64` because that is the ABI's type, and a
    /// nanosecond count is precision `Seconds` (`f32`) cannot carry.
    pub system_time_ns: Option<i64>,
}

/// Returns whether a transport `f64` may be advertised to a plugin as valid.
///
/// Every plugin API has per-field "this value is filled in" flags, and setting
/// one for a NaN or an infinity is worse than leaving it clear: a plugin that
/// trusts the flag does arithmetic with the value, and NaN propagates straight
/// through its timing math into the audio buffer. A cleared flag makes the
/// plugin fall back to its own defaults, which is always recoverable.
///
/// Every format host uses this one gate. This is the finiteness half only. A field with an additional domain rule
/// (tempo must also be positive) applies that at the call site, since the rule
/// is per-field rather than per-type.
pub fn is_usable(value: f64) -> bool {
    value.is_finite()
}

/// Playback / record / cycle flags.
///
/// Named `TransportFlags` after the plugin-SDK term for exactly this bundle —
/// CLAP and VST2 both call the field `flags`. Distinct from tutti-core's
/// `TransportState` *trait* (a live-timeline reader, which no plugin node
/// consumes: each reads its block's transport from the graph); this is the
/// wire snapshot.
#[derive(Debug, Clone, Copy, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TransportFlags {
    /// `true` while the transport is rolling.
    pub playing: bool,
    /// `true` while the host is capturing. Independent of
    /// [`playing`](Self::playing) — a punch-in armed but not yet rolling is
    /// recording without playing.
    pub recording: bool,
    /// `true` when cycle/loop mode is on, which is what makes
    /// [`LoopRegion`] meaningful.
    pub cycle_active: bool,
}

/// Tempo and time signature at the playhead.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct MusicalTiming {
    /// Tempo in BPM at the playhead. A raw `f64` rather than `Bpm` because it
    /// is handed straight to a C ABI, which is where the unit types stop. Must
    /// be finite *and* positive before a format flags it valid — see
    /// [`is_usable`], which covers only the finiteness half.
    pub tempo: f64,
    /// The signature in force at the playhead.
    ///
    /// A [`TimeSignature`] rather than a loose `(i32, i32)` pair: each format
    /// wants a different width (CLAP `u16`, VST2/VST3 `i32`), and casting at
    /// three separate boundaries invites `as u16` on a signed value, where a
    /// negative numerator becomes 65535. The conversions live on the type and
    /// validate on the way through.
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

/// The playhead position in several coordinate systems.
///
/// Hosts populate whichever the underlying plugin format understands; each
/// format host reads only the fields it needs.
#[derive(Debug, Clone, Copy, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TransportPosition {
    /// Sample-accurate project timeline position (vst2 `samplePos`; vst3
    /// `projectTimeSamples`; AU `outCurrentSampleInTimeLine`). Jumps when the
    /// transport loops or relocates.
    ///
    /// The engine's plugin node keeps it as state, as CLAP's
    /// [`seconds`](Self::seconds) (the same time, in frames, rounded): it
    /// rolls by the frames rendered while the transport plays, through tempo
    /// changes, holds while it is stopped (a stop, and a start where it
    /// stopped, continue it), and is re-seated from the playhead's beat at
    /// the tempo in force (`beats * 60 / tempo * sample_rate`) only at a jump:
    /// the first block, a seek (stopped or playing), a start somewhere other
    /// than where it stood, a loop wrap, a loop edit that moves the
    /// playhead, or a sample-rate change (a loop edit that leaves the
    /// playhead where it was, and a stop and a start inside one chunk,
    /// continue it). The engine has no tempo map, so after a tempo change
    /// the two no longer agree; the rolled count is the time actually
    /// played. A tempo change in the same chunk as a stop or a start is
    /// counted at one of the two tempos, off by up to
    /// `chunk · |1 − ratio|` frames until the next re-seat.
    ///
    /// `None` means the producer has no project-time position to report (no
    /// usable tempo, or a hand-built snapshot). It is an `Option` rather than
    /// a plain `i64` because neither the VST2 nor the VST3 ABI has a validity
    /// bit for its sample-position field, so a placeholder `0` would travel
    /// as fact; the `Option` makes each format host decide what to send.
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
    /// Seconds from song start (clap `song_pos_seconds`): the engine's
    /// plugin node fills it as [`samples`](Self::samples) over the rate,
    /// unrounded. A raw `f64` rather
    /// than `Seconds` because it is handed straight to a C ABI, which is where
    /// the unit types stop — and `Seconds` is `f32`-backed, so it cannot carry
    /// an hour-long song position at full precision.
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

/// Where the current bar starts, and its number.
#[derive(Debug, Clone, Copy, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BarInfo {
    /// Position of the current bar in quarter notes (vst2/vst3
    /// `barPositionMusic`). A raw `f64` rather than `Beat` because it is
    /// handed straight to a C ABI, which is where the unit types stop.
    pub position_quarters: f64,
    /// Position of the current bar in beats (clap `bar_start`).
    pub start_beats: f64,
    /// 1-based bar number (clap `bar_number`).
    pub number: BarNumber,
}

impl Default for TransportInfo {
    /// Returns a stopped transport at 120 BPM, 4/4, with a zero sample rate.
    fn default() -> Self {
        Self {
            state: TransportFlags::default(),
            timing: MusicalTiming::default(),
            position: TransportPosition::default(),
            loop_region: LoopRegion::default(),
            bar: BarInfo::default(),
            sample_rate: 0.0,
            system_time_ns: None,
        }
    }
}

impl TransportInfo {
    /// Creates the default transport: stopped, 120 BPM, 4/4.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets tempo in BPM (builder). Not validated here — see [`is_usable`].
    pub fn with_tempo(mut self, tempo: f64) -> Self {
        self.timing.tempo = tempo;
        self
    }

    /// Sets the playing flag (builder).
    pub fn with_playing(mut self, playing: bool) -> Self {
        self.state.playing = playing;
        self
    }

    /// Sets the recording flag (builder). Independent of the playing flag.
    pub fn with_recording(mut self, recording: bool) -> Self {
        self.state.recording = recording;
        self
    }

    /// Sets the time signature at the playhead (builder).
    pub fn with_time_signature(mut self, signature: TimeSignature) -> Self {
        self.timing.signature = signature;
        self
    }

    /// Sets the render rate in Hz (builder).
    pub fn with_sample_rate(mut self, sample_rate: f64) -> Self {
        self.sample_rate = sample_rate;
        self
    }

    /// Sets the CLAP-style position (beats and seconds) (builder).
    pub fn with_position_beats(mut self, beats: f64, seconds: f64) -> Self {
        self.position.beats = beats;
        self.position.seconds = seconds;
        self
    }

    /// Sets the VST-style musical position in quarter notes (builder).
    ///
    /// Does **not** set a sample position: the producer reports that
    /// separately via [`with_position_samples`](Self::with_position_samples),
    /// and one that has none leaves [`TransportPosition::samples`] `None`.
    pub fn with_position_quarters(mut self, quarters: f64) -> Self {
        self.position.quarters = quarters;
        self
    }

    /// Sets the project-time sample position (vst2 `samplePos`; vst3
    /// `projectTimeSamples`), the one that jumps on loop/relocate (builder).
    ///
    /// See [`TransportPosition::samples`] for how the engine derives it.
    pub fn with_position_samples(mut self, samples: i64) -> Self {
        self.position.samples = Some(samples);
        self
    }

    /// Sets the monotonic continuous sample counter (vst3
    /// `continousTimeSamples`, clap `steady_time`), the one that does not reset
    /// on loop (builder).
    ///
    /// Left at `0`, consumers fall back to the project-time `samples`.
    pub fn with_continuous_samples(mut self, continuous_samples: i64) -> Self {
        self.position.continuous_samples = continuous_samples;
        self
    }

    /// Sets the host's monotonic clock reading, in nanoseconds (builder). See
    /// [`TransportInfo::system_time_ns`].
    pub fn with_system_time_ns(mut self, nanoseconds: i64) -> Self {
        self.system_time_ns = Some(nanoseconds);
        self
    }

    /// Frames from the snapshot's first frame to the nearest MIDI clock, at
    /// 24 clocks per quarter note (vst2 `samplesToNextClock`, vst3
    /// `samplesToNextClock`): negative when the nearest clock is behind,
    /// as both SDKs define it ("can be negative (nearest)").
    ///
    /// Derived from the musical position, the tempo and the rate, so it is
    /// the grid the [`quarters`](TransportPosition::quarters) the plugin is
    /// told already implies. `None` unless all three are usable: a finite
    /// position, a finite positive tempo and rate (see [`is_usable`]).
    ///
    /// # Examples
    ///
    /// ```
    /// use tutti_plugin_types::TransportInfo;
    ///
    /// // 120 BPM at 48 kHz: a quarter note is 24 000 frames, a clock 1 000.
    /// let t = TransportInfo::new()
    ///     .with_sample_rate(48_000.0)
    ///     .with_tempo(120.0)
    ///     .with_position_quarters(2.0 + 0.25 / 24.0);
    /// assert_eq!(t.samples_to_nearest_clock(), Some(-250));
    /// ```
    pub fn samples_to_nearest_clock(&self) -> Option<i32> {
        const CLOCKS_PER_QUARTER: f64 = 24.0;
        let (quarters, tempo, rate) = (self.position.quarters, self.timing.tempo, self.sample_rate);
        if !(is_usable(quarters) && is_usable(tempo) && is_usable(rate))
            || tempo <= 0.0
            || rate <= 0.0
        {
            return None;
        }
        let clocks = quarters * CLOCKS_PER_QUARTER;
        let to_nearest = (clocks.round() - clocks) / CLOCKS_PER_QUARTER;
        let frames = (to_nearest * 60.0 / tempo * rate).round();
        // Half a clock at the slowest tempo a plugin is told is far inside
        // `i32`; a pathological tempo saturates rather than wraps.
        Some(frames.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32)
    }

    /// Sets every bar field (builder).
    ///
    /// `position_quarters` and `start_beats` take the same value because the
    /// engine measures both in quarter notes — the distinction exists only for
    /// hosts whose CLAP beat axis is notated beats rather than quarters. VST2
    /// and VST3 read `position_quarters`; CLAP reads `start_beats`.
    pub fn with_bar(mut self, start_quarters: f64, number: BarNumber) -> Self {
        self.bar.position_quarters = start_quarters;
        self.bar.start_beats = start_quarters;
        self.bar.number = number;
        self
    }

    /// Sets every loop field, plus the cycle-active flag (builder).
    ///
    /// The `_beats` and `_quarters` pairs take the same value for the same
    /// reason [`with_bar`](Self::with_bar) does. VST2 and VST3 read the
    /// `_quarters` pair; CLAP reads the `_beats` pair.
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

    /// `with_loop` must fill both pairs, exactly as `with_bar` does. Filling
    /// only `_beats` leaves VST2/VST3 — which read `_quarters` — seeing 0..0
    /// while their cycle-valid bit is set anyway, and only CLAP sees the loop.
    #[test]
    fn with_loop_fills_quarters_as_well_as_beats() {
        let t = TransportInfo::new().with_loop(true, 4.0, 16.0);

        assert!(t.state.cycle_active);
        assert_eq!(t.loop_region.start_beats, 4.0);
        assert_eq!(t.loop_region.end_beats, 16.0);
        assert_eq!(t.loop_region.start_quarters, 4.0);
        assert_eq!(t.loop_region.end_quarters, 16.0);
    }

    /// A snapshot nobody reported a project-time position on carries `None`,
    /// not a `0` that VST2/VST3 would forward as fact; the builder that
    /// reports one sets it, and only that builder.
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

    /// **The MIDI clock offset is the distance to the nearest 24-ppq clock,
    /// in frames, signed.** At 120 BPM and 48 kHz a clock is 1 000 frames;
    /// at 90 BPM and 44.1 kHz it is 1 225. A quarter of a clock past a clock
    /// is behind it (negative), three quarters is ahead of the next.
    ///
    /// Mutation: `clocks.ceil()` for `clocks.round()` (the *next* clock) →
    /// the -250 case reads 750 → fails. Mutation: drop the `tempo <= 0.0`
    /// gate → a zero tempo saturates to `Some(i32::MAX)` → fails. Mutation:
    /// `/ CLOCKS_PER_QUARTER` dropped from `to_nearest` → 24 times too far →
    /// fails.
    #[test]
    fn samples_to_nearest_clock_is_the_signed_distance_on_the_24_ppq_grid() {
        let at = |tempo: f64, rate: f64, quarters: f64| {
            TransportInfo::new()
                .with_tempo(tempo)
                .with_sample_rate(rate)
                .with_position_quarters(quarters)
                .samples_to_nearest_clock()
        };
        assert_eq!(at(120.0, 48_000.0, 2.0), Some(0));
        assert_eq!(at(120.0, 48_000.0, 2.0 + 0.25 / 24.0), Some(-250));
        assert_eq!(at(120.0, 48_000.0, 2.0 + 0.75 / 24.0), Some(250));
        assert_eq!(at(90.0, 44_100.0, 1.0 + 0.4 / 24.0), Some(-490));
        assert_eq!(at(90.0, 44_100.0, 1.0 + 0.6 / 24.0), Some(490));

        // Nothing to derive it from.
        assert_eq!(at(0.0, 48_000.0, 2.0), None);
        assert_eq!(at(-120.0, 48_000.0, 2.0), None);
        assert_eq!(at(120.0, 0.0, 2.0), None);
        assert_eq!(at(120.0, 48_000.0, f64::NAN), None);
        assert_eq!(at(f64::INFINITY, 48_000.0, 2.0), None);
    }
}
